use axum::{extract::State, response::IntoResponse, Json};
use serde_json::{json, Value};
use std::{
    cmp::Ordering,
    collections::HashMap,
    fs,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

use crate::bot::state::TradingBot;

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/maker/state
// ─────────────────────────────────────────────────────────────────────────────
pub async fn maker_state(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    let (mode, execution_mode) = {
        let cfg = bot.control_plane.config.read().await;
        (cfg.mode.clone(), cfg.execution_mode.clone())
    };
    let l2_feed_stale = bot.l2_feed_stale();
    let l2_feed_age_ms = bot.l2_feed_age_ms().await;
    let snapshot = bot.maker_state_snapshot().await;
    let feed_truth = compact_feed_truth(&snapshot, l2_feed_stale, l2_feed_age_ms);
    let no_trade_truth = compact_no_trade_truth(&snapshot, l2_feed_stale);

    Json(json!({
        "mode": mode,
        "execution_mode": execution_mode,
        "l2_feed_stale": l2_feed_stale,
        "l2_feed_age_ms": l2_feed_age_ms,
        "feed_truth": feed_truth,
        "no_trade_truth": no_trade_truth,
        "maker": snapshot,
    }))
}

fn candidate_label_from_args(args: &Value) -> String {
    let mode = args
        .get("candidate_mode")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if mode.eq_ignore_ascii_case("widen") {
        if let Some(bps) = args
            .get("maker_backwardation_widen_bps")
            .and_then(Value::as_f64)
        {
            let floor_bps = args
                .get("maker_backwardation_min_target_spread_bps")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            return widen_candidate_label(bps, floor_bps);
        }
    }
    if mode.eq_ignore_ascii_case("roll_guard") {
        let conf = args
            .get("maker_roll_toxic_min_confidence")
            .and_then(Value::as_f64)
            .unwrap_or(0.15);
        let signal = args
            .get("maker_roll_toxic_min_signal")
            .and_then(Value::as_f64)
            .unwrap_or(0.05);
        let samples = args
            .get("maker_roll_toxic_min_sample_count")
            .and_then(Value::as_u64)
            .unwrap_or(6);
        return roll_guard_candidate_label(conf, signal, samples);
    }
    mode.to_string()
}

fn widen_candidate_label(widen_bps: f64, floor_bps: f64) -> String {
    let mut label = format!("widen_{widen_bps:.0}bps");
    if floor_bps > 0.0 {
        label.push_str(&format!("_floor{floor_bps:.0}bps"));
    }
    label
}

fn roll_guard_candidate_label(confidence: f64, signal: f64, samples: u64) -> String {
    format!(
        "roll_guard_c{:.0}_s{:.0}_n{}",
        confidence * 100.0,
        signal * 100.0,
        samples
    )
}

fn as_u64(value: Option<&Value>) -> Option<u64> {
    value.and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_i64().and_then(|n| u64::try_from(n).ok()))
    })
}

fn as_f64(value: Option<&Value>) -> Option<f64> {
    value.and_then(|v| {
        v.as_f64()
            .or_else(|| v.as_i64().map(|n| n as f64))
            .or_else(|| v.as_u64().map(|n| n as f64))
    })
}

fn summarize_batch_artifact(path: &FsPath, value: &Value) -> Value {
    let args = value.get("args").unwrap_or(&Value::Null);
    let counts = value.get("counts").unwrap_or(&Value::Null);
    let aggregates = value.get("aggregates").unwrap_or(&Value::Null);
    let run_control = value.get("run_control").unwrap_or(&Value::Null);
    let verdict = value
        .get("verdict")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let candidate_label = candidate_label_from_args(args);

    json!({
        "path": path.to_string_lossy(),
        "generated_at": value.get("generated_at").and_then(Value::as_str),
        "verdict": verdict,
        "candidate_label": candidate_label,
        "candidate_mode": args.get("candidate_mode").and_then(Value::as_str),
        "maker_backwardation_widen_bps": as_f64(args.get("maker_backwardation_widen_bps")),
        "maker_backwardation_min_target_spread_bps": as_f64(
            args.get("maker_backwardation_min_target_spread_bps")
        ),
        "cycles_run": as_u64(value.get("cycles_run")),
        "usable_cycles": as_u64(counts.get("usable_cycles")),
        "sparse_cycles": as_u64(counts.get("sparse_cycles")),
        "engaged_comparable_windows": as_u64(counts.get("engaged_comparable_windows")),
        "engaged_improved_adverse_move": as_u64(counts.get("engaged_improved_adverse_move")),
        "engaged_regressed_adverse_move": as_u64(counts.get("engaged_regressed_adverse_move")),
        "avg_adverse_move_delta_bps": as_f64(aggregates.get("avg_engaged_adverse_move_delta_bps"))
            .or_else(|| as_f64(aggregates.get("avg_adverse_move_delta_bps"))),
        "avg_p90_adverse_delta_bps": as_f64(aggregates.get("avg_p90_adverse_delta_bps")),
        "avg_places_delta_pct": as_f64(aggregates.get("avg_places_delta_pct")),
        "avg_widen_count": as_f64(aggregates.get("avg_widen_count")),
        "stop_reason": run_control.get("stop_reason").and_then(Value::as_str),
        "next_eligible_delay_seconds": as_f64(run_control.get("next_eligible_delay_seconds")),
    })
}

fn load_batch_summaries(dir: &FsPath) -> Vec<Value> {
    let mut summaries = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return summaries;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if value.get("verdict").and_then(Value::as_str).is_none() {
            continue;
        }
        summaries.push(summarize_batch_artifact(&path, &value));
    }

    summaries.sort_by(|a, b| {
        let a_ts = a.get("generated_at").and_then(Value::as_str).unwrap_or("");
        let b_ts = b.get("generated_at").and_then(Value::as_str).unwrap_or("");
        b_ts.cmp(a_ts)
    });
    summaries
}

fn load_latest_json_matching(dir: &FsPath, needle: &str) -> Option<(PathBuf, Value)> {
    let stable_latest = dir.join(format!("latest{needle}.json"));
    if stable_latest.exists() {
        let Ok(raw) = fs::read_to_string(&stable_latest) else {
            return None;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            return None;
        };
        return Some((stable_latest, value));
    }

    let Ok(entries) = fs::read_dir(dir) else {
        return None;
    };

    let mut matches: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().and_then(|s| s.to_str()) == Some("json")
                && path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|name| name.contains(needle))
        })
        .collect();
    matches.sort();
    matches.reverse();

    for path in matches {
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        return Some((path, value));
    }
    None
}

fn compact_label_note(value: &Value) -> Value {
    json!({
        "label": value.get("label").and_then(Value::as_str),
        "source": value.get("source").and_then(Value::as_str),
        "note": value.get("note").and_then(Value::as_str),
        "evidence": value.get("evidence").cloned(),
    })
}

fn compact_feed_truth(snapshot: &Value, l2_feed_stale: bool, l2_feed_age_ms: Option<u64>) -> Value {
    let l2_summary = snapshot.get("l2_summary").unwrap_or(&Value::Null);
    json!({
        "l2_feed_stale": l2_feed_stale,
        "l2_feed_age_ms": l2_feed_age_ms,
        "market_count": snapshot.get("market_count").and_then(Value::as_u64),
        "quote_count": snapshot.get("quote_count").and_then(Value::as_u64),
        "target_market_count": l2_summary.get("target_market_count").and_then(Value::as_u64),
        "token_count": l2_summary.get("token_count").and_then(Value::as_u64),
        "snapshot_loaded_count": l2_summary.get("snapshot_loaded_count").and_then(Value::as_u64),
        "two_sided_count": l2_summary.get("two_sided_count").and_then(Value::as_u64),
        "one_sided_count": l2_summary.get("one_sided_count").and_then(Value::as_u64),
        "stale_count": l2_summary.get("stale_count").and_then(Value::as_u64),
        "quoteable_count": l2_summary.get("quoteable_count").and_then(Value::as_u64),
        "primary_not_ready_reason": l2_summary.get("primary_not_ready_reason").and_then(Value::as_str),
        "not_ready_reasons": l2_summary.get("not_ready_reasons").cloned(),
        "l2_stale_code": snapshot.get("l2_stale_code").and_then(Value::as_str),
        "l2_stale_reason": snapshot.get("l2_stale_reason").and_then(Value::as_str),
    })
}

fn normalized_l2_stale_code(snapshot: &Value) -> Option<&str> {
    snapshot
        .get("l2_stale_code")
        .and_then(Value::as_str)
        .or_else(|| {
            let reason = snapshot.get("l2_stale_reason").and_then(Value::as_str)?;
            if reason == "no target markets" {
                Some("no_target_markets")
            } else if reason == "execution_mode=taker" {
                Some("execution_mode_taker")
            } else if reason.starts_with("warmup: awaiting snapshots") {
                Some("warmup_awaiting_snapshots")
            } else if reason.starts_with("warmup timeout") {
                Some("warmup_timeout")
            } else if reason.starts_with("warmup ") {
                Some("warmup_partial")
            } else if reason.starts_with("rearm_wait_ms=") {
                Some("rearm_wait")
            } else if reason.starts_with("stale timeout") {
                Some("stale_timeout")
            } else if reason.starts_with("session error:") {
                Some("session_error")
            } else if reason.starts_with("execution_actor_backpressure") {
                Some("actor_backpressure")
            } else {
                Some("l2_stale")
            }
        })
}

fn derive_no_trade_label(snapshot: &Value, l2_feed_stale: bool) -> String {
    let l2_summary = snapshot.get("l2_summary").unwrap_or(&Value::Null);
    let target_market_count = l2_summary
        .get("target_market_count")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            snapshot
                .get("market_count")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        });
    let token_count = l2_summary
        .get("token_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let snapshot_loaded_count = l2_summary
        .get("snapshot_loaded_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let two_sided_count = l2_summary
        .get("two_sided_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let one_sided_count = l2_summary
        .get("one_sided_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let stale_count = l2_summary
        .get("stale_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let primary_not_ready_reason = l2_summary
        .get("primary_not_ready_reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    let l2_stale_code = normalized_l2_stale_code(snapshot).unwrap_or("");
    let reason = snapshot
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or("");

    if l2_stale_code == "execution_mode_taker" {
        return "execution_mode_taker".to_string();
    }
    if target_market_count == 0 {
        return "no_target_markets".to_string();
    }
    if l2_feed_stale || snapshot.get("l2_stale").and_then(Value::as_bool) == Some(true) {
        return "global_l2_feed_stale".to_string();
    }
    if primary_not_ready_reason == "snapshot_missing"
        || (snapshot_loaded_count == 0 && token_count > 0)
    {
        return "no_snapshot".to_string();
    }
    if primary_not_ready_reason == "one_sided_book" || (one_sided_count > 0 && two_sided_count == 0)
    {
        return "one_sided_books".to_string();
    }
    if primary_not_ready_reason == "token_bbo_stale"
        || (stale_count > 0 && two_sided_count == 0)
        || l2_stale_code == "stale_timeout"
    {
        return "token_bbo_stale".to_string();
    }
    if primary_not_ready_reason.starts_with("l2_stale(") || !l2_stale_code.is_empty() {
        return "global_l2_feed_stale".to_string();
    }
    if reason.contains("no_quote_plan") {
        return "no_quote_plan".to_string();
    }
    if reason.contains("basis_guard") {
        return "basis_guard".to_string();
    }
    if reason.contains("entry_ineligible") {
        return "entry_ineligible".to_string();
    }
    if reason.contains("maker_buy_no") {
        return "maker_buy_no".to_string();
    }
    "maker_idle".to_string()
}

fn compact_no_trade_truth(snapshot: &Value, l2_feed_stale: bool) -> Value {
    let l2_summary = snapshot.get("l2_summary").unwrap_or(&Value::Null);
    let label = derive_no_trade_label(snapshot, l2_feed_stale);
    json!({
        "label": label,
        "decision": snapshot.get("decision").and_then(Value::as_str),
        "bid_action": snapshot.get("bid_action").and_then(Value::as_str),
        "bid_reason": snapshot.get("bid_reason").and_then(Value::as_str),
        "ask_action": snapshot.get("ask_action").and_then(Value::as_str),
        "ask_reason": snapshot.get("ask_reason").and_then(Value::as_str),
        "target_market_count": l2_summary.get("target_market_count").and_then(Value::as_u64),
        "primary_not_ready_reason": l2_summary.get("primary_not_ready_reason").and_then(Value::as_str),
        "not_ready_reasons": l2_summary.get("not_ready_reasons").cloned(),
        "l2_stale_code": normalized_l2_stale_code(snapshot),
        "l2_stale_reason": snapshot.get("l2_stale_reason").and_then(Value::as_str),
        "market_count": snapshot.get("market_count").and_then(Value::as_u64),
        "quote_count": snapshot.get("quote_count").and_then(Value::as_u64),
        "snapshot_loaded_count": l2_summary.get("snapshot_loaded_count").and_then(Value::as_u64),
        "two_sided_count": l2_summary.get("two_sided_count").and_then(Value::as_u64),
        "one_sided_count": l2_summary.get("one_sided_count").and_then(Value::as_u64),
        "stale_count": l2_summary.get("stale_count").and_then(Value::as_u64),
        "quoteable_count": l2_summary.get("quoteable_count").and_then(Value::as_u64),
        "market_id_count": snapshot
            .get("inventory_by_market")
            .and_then(Value::as_object)
            .map(|obj| obj.len()),
    })
}

fn compact_shutdown_attempt(value: &Value) -> Value {
    json!({
        "db": value.get("db").and_then(Value::as_str).map(|s| PathBuf::from(s).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| s.to_string())),
        "classification": value.get("classification").cloned(),
        "trade_count": value.get("trade_count"),
        "open_trade_count": value.get("open_trade_count"),
        "closed_trade_count": value.get("closed_trade_count"),
        "fill_count": value.get("fill_count"),
        "integrity_status": value.get("integrity_status").and_then(Value::as_str),
        "integrity_verdict": value.get("integrity_verdict").and_then(Value::as_str),
        "integrity_reason": value.get("integrity_reason").and_then(Value::as_str),
        "run_classification": value
            .get("run_classification")
            .and_then(|v| v.get("label"))
            .and_then(Value::as_str),
        "primary_blocker": value
            .get("primary_blocker_summary")
            .and_then(|v| v.get("label"))
            .and_then(Value::as_str),
    })
}

fn summarize_shutdown_validation_bundle(value: &Value) -> Value {
    let run_classification = value.get("run_classification").unwrap_or(&Value::Null);
    let primary_blocker = value.get("primary_blocker_summary").unwrap_or(&Value::Null);
    let shutdown_summary = value.get("shutdown_summary").unwrap_or(&Value::Null);
    json!({
        "terminal_class": value.get("terminal_class").and_then(Value::as_str),
        "run_classification": run_classification.get("label").and_then(Value::as_str),
        "run_classification_note": run_classification.get("note").and_then(Value::as_str),
        "primary_blocker_summary": compact_label_note(primary_blocker),
        "primary_blocker_label": primary_blocker.get("label").and_then(Value::as_str),
        "cleanup_trade_count": shutdown_summary.get("cleanup_trade_count"),
        "integrity_status": shutdown_summary.get("integrity_status").and_then(Value::as_str),
        "integrity_verdict": shutdown_summary.get("integrity_verdict").and_then(Value::as_str),
        "integrity_reason": shutdown_summary.get("integrity_reason").and_then(Value::as_str),
        "peak_active_trade_count": shutdown_summary.get("peak_active_trade_count"),
        "stop_on_open_target_reached": shutdown_summary.get("stop_on_open_target_reached"),
        "run_exit_cause": shutdown_summary.get("run_exit_cause").and_then(Value::as_str),
        "open_trade_count": shutdown_summary.get("open_trade_count"),
        "closing_trade_count": shutdown_summary.get("closing_trade_count"),
        "trade_count": shutdown_summary.get("trade_count"),
    })
}

fn summarize_shutdown_validation_attempts(value: &Value) -> Value {
    let classification_counts = value
        .get("classification_counts")
        .cloned()
        .unwrap_or(Value::Null);
    let attempts = value
        .get("attempts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let latest_attempt = attempts
        .first()
        .map(compact_shutdown_attempt)
        .unwrap_or(Value::Null);
    json!({
        "attempt_count": value.get("attempt_count"),
        "classification_counts": classification_counts,
        "detail_counts_top": value.get("detail_counts_top"),
        "latest_attempt": latest_attempt,
        "proof_count": classification_counts.get("proof"),
        "starvation_count": classification_counts.get("starvation"),
        "other_count": classification_counts.get("other"),
    })
}

fn is_front_runner_eligible(verdict: &str) -> bool {
    matches!(
        verdict,
        "mixed_when_engaged"
            | "directional_improvement_when_engaged"
            | "promising_but_noncomparable"
            | "promising_guard_candidate"
            | "promising_repeatable_candidate"
    )
}

fn is_meaningful_batch_verdict(verdict: &str) -> bool {
    !matches!(
        verdict,
        "awaiting_eligible_start" | "inadequate_sample_batch" | "insufficient_batch" | "unknown"
    )
}

fn front_runner_cost(summary: &Value) -> f64 {
    let adverse = summary
        .get("avg_adverse_move_delta_bps")
        .and_then(Value::as_f64)
        .unwrap_or(10_000.0);
    let p90 = summary
        .get("avg_p90_adverse_delta_bps")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let places_tax = summary
        .get("avg_places_delta_pct")
        .and_then(Value::as_f64)
        .map(f64::abs)
        .unwrap_or(1000.0);
    adverse + (0.20 * places_tax) + (0.05 * p90)
}

fn compare_front_runner(a: &Value, b: &Value) -> Ordering {
    let a_cost = front_runner_cost(a);
    let b_cost = front_runner_cost(b);
    a_cost
        .partial_cmp(&b_cost)
        .unwrap_or(Ordering::Equal)
        .then_with(|| {
            let a_engaged = a
                .get("engaged_comparable_windows")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let b_engaged = b
                .get("engaged_comparable_windows")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            b_engaged.cmp(&a_engaged)
        })
        .then_with(|| {
            let a_imp = a
                .get("engaged_improved_adverse_move")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let a_reg = a
                .get("engaged_regressed_adverse_move")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let b_imp = b
                .get("engaged_improved_adverse_move")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let b_reg = b
                .get("engaged_regressed_adverse_move")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            (b_imp as i64 - b_reg as i64).cmp(&(a_imp as i64 - a_reg as i64))
        })
}

fn load_rollout_candidate_label() -> Option<String> {
    let state_path = FsPath::new("ops/profiles/maker_rollout_state.json");
    let raw = fs::read_to_string(state_path).ok()?;
    let state = serde_json::from_str::<Value>(&raw).ok()?;
    if state
        .get("active_candidate_label")
        .and_then(Value::as_str)
        .is_some()
    {
        return state
            .get("active_candidate_label")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    let profile_rel = state
        .get("active_candidate_profile")
        .and_then(Value::as_str)?;
    let raw = fs::read_to_string(profile_rel).ok()?;
    let cfg = serde_json::from_str::<Value>(&raw).ok()?;
    let widen = cfg
        .get("maker_backwardation_widen_bps")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let floor = cfg
        .get("maker_backwardation_min_target_spread_bps")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if widen > 0.0 {
        Some(widen_candidate_label(widen, floor))
    } else if cfg
        .get("maker_roll_toxic_guard_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Some(roll_guard_candidate_label(
            cfg.get("maker_roll_toxic_min_confidence")
                .and_then(Value::as_f64)
                .unwrap_or(0.15),
            cfg.get("maker_roll_toxic_min_signal")
                .and_then(Value::as_f64)
                .unwrap_or(0.05),
            cfg.get("maker_roll_toxic_min_sample_count")
                .and_then(Value::as_u64)
                .unwrap_or(6),
        ))
    } else {
        None
    }
}

fn select_front_runner(
    latest_by_candidate: &[Value],
    preferred_label: Option<&str>,
) -> Option<Value> {
    let mut candidates: Vec<Value> = latest_by_candidate
        .iter()
        .filter(|summary| {
            let verdict = summary
                .get("verdict")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            is_front_runner_eligible(verdict)
        })
        .cloned()
        .collect();
    if let Some(label) = preferred_label {
        let preferred: Vec<Value> = candidates
            .iter()
            .filter(|summary| summary.get("candidate_label").and_then(Value::as_str) == Some(label))
            .cloned()
            .collect();
        candidates = preferred;
    }
    candidates.sort_by(compare_front_runner);
    candidates.into_iter().next()
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/maker/ab-summary
// ─────────────────────────────────────────────────────────────────────────────
pub async fn maker_ab_summary() -> impl IntoResponse {
    let dir = PathBuf::from("artifacts/maker-ab-batches");
    let summaries = load_batch_summaries(&dir);
    if summaries.is_empty() {
        return Json(json!({
            "status": "no_artifacts",
            "dir": dir.to_string_lossy(),
            "latest": Value::Null,
            "latest_by_candidate": [],
            "front_runner": Value::Null,
            "notes": [
                "No maker A/B batch artifacts found yet."
            ],
        }));
    }

    let latest = summaries.first().cloned();
    let mut latest_by_candidate_map: HashMap<String, Value> = HashMap::new();
    for summary in &summaries {
        let label = summary
            .get("candidate_label")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        latest_by_candidate_map
            .entry(label)
            .or_insert_with(|| summary.clone());
    }
    let mut latest_by_candidate: Vec<Value> = latest_by_candidate_map.into_values().collect();
    latest_by_candidate.sort_by(compare_front_runner);
    let mut latest_meaningful_by_candidate: Vec<Value> = latest_by_candidate
        .iter()
        .filter(|summary| {
            summary
                .get("verdict")
                .and_then(Value::as_str)
                .is_some_and(is_meaningful_batch_verdict)
        })
        .cloned()
        .collect();
    latest_meaningful_by_candidate.sort_by(compare_front_runner);
    let rollout_candidate_label = load_rollout_candidate_label();
    let front_runner = if rollout_candidate_label.is_some() {
        select_front_runner(
            &latest_meaningful_by_candidate,
            rollout_candidate_label.as_deref(),
        )
    } else {
        None
    };
    let reference_front_runner = select_front_runner(&latest_meaningful_by_candidate, None);

    let mut notes = Vec::new();
    if let Some(latest) = latest.as_ref() {
        if latest.get("verdict").and_then(Value::as_str) == Some("awaiting_eligible_start") {
            if let Some(delay) = latest
                .get("next_eligible_delay_seconds")
                .and_then(Value::as_f64)
            {
                notes.push(format!(
                    "Latest batch is waiting for the next eligible start window in about {:.0} seconds.",
                    delay
                ));
            } else {
                notes.push(
                    "Latest batch is waiting for the next eligible start window.".to_string(),
                );
            }
        }
    }
    if rollout_candidate_label.is_none() {
        notes.push(
            "No active maker rollout candidate is selected; widening remains retired research-only."
                .to_string(),
        );
    }
    if let (Some(front), Some(latest)) = (front_runner.as_ref(), latest.as_ref()) {
        let front_label = front
            .get("candidate_label")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let latest_label = latest
            .get("candidate_label")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if front_label != latest_label {
            notes.push(format!(
                "Current front-runner is `{front_label}`, while the latest artifact came from `{latest_label}`."
            ));
        }
    }
    if let (Some(front), Some(reference)) = (front_runner.as_ref(), reference_front_runner.as_ref())
    {
        let front_label = front
            .get("candidate_label")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let reference_label = reference
            .get("candidate_label")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if front_label != reference_label {
            notes.push(format!(
                "Frozen rollout candidate is `{front_label}`; best overall historical batch is still `{reference_label}`."
            ));
        }
    }

    Json(json!({
        "status": "ok",
        "dir": dir.to_string_lossy(),
        "latest": latest,
        "latest_by_candidate": latest_by_candidate,
        "latest_meaningful_by_candidate": latest_meaningful_by_candidate,
        "front_runner": front_runner,
        "reference_front_runner": reference_front_runner,
        "notes": notes,
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/maker/rollout-status
// ─────────────────────────────────────────────────────────────────────────────
pub async fn maker_rollout_status() -> impl IntoResponse {
    let dir = PathBuf::from("artifacts/readiness-reports");
    let readiness = load_latest_json_matching(&dir, "_maker_rollout_readiness_report");
    let gate = load_latest_json_matching(&dir, "_maker_rollout_gate");
    let gate_runtime = load_latest_json_matching(&dir, "_maker_rollout_gate_status");
    let shutdown_dir = PathBuf::from("artifacts/shutdown-validation-attempts");
    let shutdown_bundle =
        load_latest_json_matching(&shutdown_dir, "_shutdown_validation_proof_bundle");
    let shutdown_attempts =
        load_latest_json_matching(&shutdown_dir, "_shutdown_validation_attempts_report");

    if readiness.is_none()
        && gate.is_none()
        && gate_runtime.is_none()
        && shutdown_bundle.is_none()
        && shutdown_attempts.is_none()
    {
        return Json(json!({
            "status": "no_artifacts",
            "dir": dir.to_string_lossy(),
            "readiness": Value::Null,
            "gate": Value::Null,
            "gate_runtime": Value::Null,
            "shutdown_validation": Value::Null,
        }));
    }

    let readiness_value = readiness.as_ref().map(|(_, value)| value.clone());
    let gate_value = gate.as_ref().map(|(_, value)| value.clone());
    let gate_runtime_value = gate_runtime.as_ref().map(|(_, value)| value.clone());
    let shutdown_bundle_value = shutdown_bundle
        .as_ref()
        .map(|(_, value)| summarize_shutdown_validation_bundle(value));
    let shutdown_attempts_value = shutdown_attempts
        .as_ref()
        .map(|(_, value)| summarize_shutdown_validation_attempts(value));

    Json(json!({
        "status": "ok",
        "dir": dir.to_string_lossy(),
        "readiness": readiness_value,
        "gate": gate_value,
        "gate_runtime": gate_runtime_value,
        "shutdown_validation": {
            "bundle_path": shutdown_bundle.as_ref().map(|(path, _)| path.to_string_lossy().to_string()),
            "attempts_path": shutdown_attempts.as_ref().map(|(path, _)| path.to_string_lossy().to_string()),
            "latest_bundle": shutdown_bundle_value,
            "latest_attempts": shutdown_attempts_value,
            "status": shutdown_bundle_value
                .as_ref()
                .and_then(|v| v.get("terminal_class"))
                .and_then(Value::as_str),
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        candidate_label_from_args, compact_feed_truth, compact_no_trade_truth,
        compare_front_runner, derive_no_trade_label, is_front_runner_eligible,
        is_meaningful_batch_verdict,
    };
    use serde_json::json;
    use std::cmp::Ordering;

    #[test]
    fn selected_candidate_without_evidence_cannot_inherit_another_candidates_result() {
        let summaries =
            vec![json!({"candidate_label":"other", "verdict":"promising_repeatable_candidate"})];
        assert!(super::select_front_runner(&summaries, Some("selected")).is_none());
        assert!(super::select_front_runner(&summaries, None).is_some());
    }

    #[test]
    fn candidate_label_uses_widen_bps() {
        let args = json!({
            "candidate_mode": "widen",
            "maker_backwardation_widen_bps": 20.0
        });
        assert_eq!(candidate_label_from_args(&args), "widen_20bps");

        let floored_args = json!({
            "candidate_mode": "widen",
            "maker_backwardation_widen_bps": 40.0,
            "maker_backwardation_min_target_spread_bps": 120.0
        });
        assert_eq!(
            candidate_label_from_args(&floored_args),
            "widen_40bps_floor120bps"
        );
    }

    #[test]
    fn front_runner_prefers_better_toxicity_after_coverage_tax() {
        let a = json!({
            "verdict": "mixed_when_engaged",
            "avg_adverse_move_delta_bps": -12.9,
            "avg_p90_adverse_delta_bps": -50.0,
            "avg_places_delta_pct": -27.0,
            "engaged_comparable_windows": 3,
            "engaged_improved_adverse_move": 2,
            "engaged_regressed_adverse_move": 1
        });
        let b = json!({
            "verdict": "mixed_when_engaged",
            "avg_adverse_move_delta_bps": -11.0,
            "avg_p90_adverse_delta_bps": -37.5,
            "avg_places_delta_pct": -31.9,
            "engaged_comparable_windows": 4,
            "engaged_improved_adverse_move": 3,
            "engaged_regressed_adverse_move": 1
        });
        assert_eq!(compare_front_runner(&a, &b), Ordering::Less);
        assert!(is_front_runner_eligible("mixed_when_engaged"));
        assert!(!is_front_runner_eligible("guard_starved_candidate"));
        assert!(!is_meaningful_batch_verdict("inadequate_sample_batch"));
    }

    #[test]
    fn no_trade_label_prefers_no_target_markets_when_market_surface_is_empty() {
        let snapshot = json!({
            "market_count": 0,
            "quote_count": 0,
            "l2_summary": {
                "target_market_count": 0,
                "token_count": 0,
                "snapshot_loaded_count": 0,
                "two_sided_count": 0,
                "one_sided_count": 0,
                "stale_count": 0,
                "quoteable_count": 0
            },
            "l2_stale_reason": "no target markets",
            "decision": "no_quote_plan"
        });

        assert_eq!(derive_no_trade_label(&snapshot, false), "no_target_markets");
        let truth = compact_no_trade_truth(&snapshot, false);
        assert_eq!(
            truth.get("label").and_then(|v| v.as_str()),
            Some("no_target_markets")
        );
        assert_eq!(truth.get("market_count").and_then(|v| v.as_u64()), Some(0));
    }

    #[test]
    fn no_trade_label_prefers_taker_mode_over_empty_market_surface() {
        let snapshot = json!({
            "market_count": 0,
            "quote_count": 0,
            "l2_stale_code": "execution_mode_taker",
            "l2_stale_reason": "execution_mode=taker",
            "l2_summary": {
                "target_market_count": 0,
                "token_count": 0,
                "snapshot_loaded_count": 0,
                "two_sided_count": 0,
                "one_sided_count": 0,
                "stale_count": 0,
                "quoteable_count": 0
            },
            "decision": "no_quote_plan"
        });

        assert_eq!(
            derive_no_trade_label(&snapshot, true),
            "execution_mode_taker"
        );
        let truth = compact_no_trade_truth(&snapshot, true);
        assert_eq!(
            truth.get("label").and_then(|v| v.as_str()),
            Some("execution_mode_taker")
        );
        assert_eq!(
            truth.get("l2_stale_code").and_then(|v| v.as_str()),
            Some("execution_mode_taker")
        );
    }

    #[test]
    fn feed_truth_compacts_l2_counts() {
        let snapshot = json!({
            "market_count": 3,
            "quote_count": 6,
            "l2_summary": {
                "target_market_count": 3,
                "token_count": 4,
                "snapshot_loaded_count": 4,
                "two_sided_count": 2,
                "one_sided_count": 1,
                "stale_count": 1,
                "quoteable_count": 1,
                "primary_not_ready_reason": "token_bbo_stale",
                "not_ready_reasons": [{"label": "token_bbo_stale", "count": 1}]
            },
            "l2_stale_code": "stale_timeout",
            "l2_stale_reason": "token_bbo_stale"
        });
        let truth = compact_feed_truth(&snapshot, true, Some(1234));
        assert_eq!(
            truth.get("l2_feed_stale").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            truth.get("snapshot_loaded_count").and_then(|v| v.as_u64()),
            Some(4)
        );
        assert_eq!(
            truth.get("quoteable_count").and_then(|v| v.as_u64()),
            Some(1)
        );
        assert_eq!(
            truth.get("target_market_count").and_then(|v| v.as_u64()),
            Some(3)
        );
        assert_eq!(
            truth
                .get("primary_not_ready_reason")
                .and_then(|v| v.as_str()),
            Some("token_bbo_stale")
        );
        assert_eq!(
            truth.get("l2_stale_code").and_then(|v| v.as_str()),
            Some("stale_timeout")
        );
        assert_eq!(
            truth.get("l2_stale_reason").and_then(|v| v.as_str()),
            Some("token_bbo_stale")
        );
    }

    #[test]
    fn no_trade_label_distinguishes_snapshot_and_one_sided_books() {
        let no_snapshot = json!({
            "market_count": 2,
            "quote_count": 0,
            "l2_summary": {
                "target_market_count": 2,
                "token_count": 2,
                "snapshot_loaded_count": 0,
                "two_sided_count": 0,
                "one_sided_count": 0,
                "stale_count": 0,
                "quoteable_count": 0,
                "primary_not_ready_reason": "snapshot_missing",
                "not_ready_reasons": [{"label": "snapshot_missing", "count": 2}]
            },
            "decision": "no_quote_plan"
        });
        assert_eq!(derive_no_trade_label(&no_snapshot, false), "no_snapshot");

        let one_sided = json!({
            "market_count": 2,
            "quote_count": 0,
            "l2_summary": {
                "target_market_count": 2,
                "token_count": 2,
                "snapshot_loaded_count": 2,
                "two_sided_count": 0,
                "one_sided_count": 2,
                "stale_count": 0,
                "quoteable_count": 0,
                "primary_not_ready_reason": "one_sided_book",
                "not_ready_reasons": [{"label": "one_sided_book", "count": 2}]
            },
            "decision": "no_quote_plan"
        });
        assert_eq!(derive_no_trade_label(&one_sided, false), "one_sided_books");
    }
}
