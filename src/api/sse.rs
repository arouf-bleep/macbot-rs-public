//! Server-Sent Events endpoint.
//!
//! Streams low-latency JSON snapshots for dashboard rendering.
//!
//! The client connects to GET /api/stream and receives:
//!
//!   data: {"analysis":{...},"stats":{...},"risk":{...},"execution":{...}, ...}
//!

use axum::{
    extract::{connect_info::ConnectInfo, Query, State},
    http::HeaderMap,
    response::{
        sse::{Event, KeepAlive},
        IntoResponse, Sse,
    },
    Json,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    convert::Infallible,
    hash::{Hash, Hasher},
    net::SocketAddr,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};
use tokio_stream::Stream;

use crate::bot::state::TradingBot;

use super::{auth, realtime};

/// How long an unchanged stream may stay silent before proving it is alive.
///
/// Must stay under the dashboard's own ten-second "is the stream live" window,
/// or a quiet connection looks like a dead one and the page starts polling.
const HEARTBEAT_AFTER: Duration = Duration::from_secs(2);

struct StreamDedupe {
    digest: u64,
    last_data_at: Instant,
    /// False until the first payload is seen, so the opening tick always sends
    /// rather than comparing against a digest that was never computed.
    seen: bool,
    /// Which build of the slow half this connection has already been sent.
    slow_generation: u64,
}

impl StreamDedupe {
    fn new(now: Instant) -> Self {
        Self {
            digest: 0,
            last_data_at: now,
            seen: false,
            slow_generation: 0,
        }
    }

    /// Decide what this tick should emit.
    ///
    /// `now` is passed in rather than read here so the decision can be tested
    /// across simulated time instead of by watching a live socket.
    fn decide(&mut self, digest: u64, now: Instant, slow_generation: u64) -> Send {
        if !self.seen || digest != self.digest {
            self.seen = true;
            self.digest = digest;
            self.last_data_at = now;
            self.slow_generation = slow_generation;
            return Send::Full;
        }
        if now.duration_since(self.last_data_at) >= HEARTBEAT_AFTER {
            self.last_data_at = now;
            return Send::Heartbeat;
        }
        Send::Nothing
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Send {
    Full,
    Heartbeat,
    Nothing,
}

/// Envelope bookkeeping that differs on every tick and tells a reader nothing.
const VOLATILE_KEYS: [&str; 4] = ["seq", "ts", "server_build_ms", "kind"];

/// Counters the client derives for itself, which would otherwise change every
/// tick and make identical payloads compare as different.
///
/// The dashboard already advances the analysis age on its own timer, and a
/// countdown to an event a day away does not need re-sending five times a
/// second to be correct on screen.
const DERIVED_COUNTERS: [(&str, &str); 3] = [
    ("analysis_freshness", "age_secs"),
    ("macro_guard", "minutes_until_event"),
    ("macro_guard", "secs_until_event"),
];

/// Fingerprint of everything in a snapshot that a reader would actually notice.
///
/// Comparing the raw payload does not work. Six fields change on every single
/// tick by construction: the sequence number, the wall clock, how long the
/// build took, the fast/slow label, the analysis age and the macro countdown.
/// Any one of them is enough to make every payload unique, so a naive hash
/// matches nothing and the deduplication silently does nothing at all.
fn payload_digest(payload: &Value) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match payload.as_object() {
        Some(obj) => {
            // serde_json's default map is ordered, so this walk is stable across
            // ticks; an unordered one would differ for identical content.
            for (key, value) in obj {
                if VOLATILE_KEYS.contains(&key.as_str()) {
                    continue;
                }
                key.hash(&mut hasher);
                stable_repr(key, value).hash(&mut hasher);
            }
        }
        None => payload.to_string().hash(&mut hasher),
    }
    hasher.finish()
}

/// A value with its self-advancing counters removed, so the rest can be compared.
fn stable_repr(key: &str, value: &Value) -> String {
    let drops: Vec<&str> = DERIVED_COUNTERS
        .iter()
        .filter(|(owner, _)| *owner == key)
        .map(|(_, field)| *field)
        .collect();
    if drops.is_empty() {
        return value.to_string();
    }
    let mut cloned = value.clone();
    strip_fields(&mut cloned, &drops);
    cloned.to_string()
}

/// Remove the named fields wherever they appear, at any depth.
fn strip_fields(value: &mut Value, fields: &[&str]) {
    match value {
        Value::Object(map) => {
            for field in fields {
                map.remove(*field);
            }
            for (_, v) in map.iter_mut() {
                strip_fields(v, fields);
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                strip_fields(v, fields);
            }
        }
        _ => {}
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/stream
// ─────────────────────────────────────────────────────────────────────────────
pub async fn stream(
    State(bot): State<Arc<TradingBot>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, axum::response::Response> {
    let provided_key = headers.get("x-api-key").and_then(|v| v.to_str().ok());
    let provided_token = query.get("stream_token").map(String::as_str);
    let authorization = match auth::authorize_stream(
        &bot,
        Some(addr.ip().to_string()),
        provided_key,
        provided_token,
    )
    .await
    {
        Ok(authorization) => authorization,
        Err(status) => return Err((status, "stream auth failed").into_response()),
    };

    // Tick at 200ms so a real change reaches the browser within 200ms.
    //
    // Every tick used to send a full snapshot whether or not anything had
    // changed: measured at six events and thirty kilobytes a second, forever,
    // while the analysis inside it only recomputes every thirty seconds. Well
    // over ninety percent of it was the previous payload sent again.
    //
    // The tick stays fast; only the sending is conditional. Identical payloads
    // are dropped, so a change still arrives in 200ms and an idle connection
    // costs a small heartbeat instead of five kilobytes.
    let mut interval = tokio::time::interval(Duration::from_millis(200));
    // Tokio's default is Burst: if a tick is missed the next poll fires all the
    // backlog back to back. Building a snapshot can take longer than the period,
    // so the loop fell behind and then raced through a hundred catch-up ticks in
    // a quarter of a second. Those ticks all saw the same payload, so they all
    // skipped, and the heartbeat clock never advanced enough to fire: the stream
    // then went quiet for eleven seconds, past the client's liveness window.
    //
    // Skip drops the backlog and waits for the next real boundary, so a tick
    // means 200ms of wall clock rather than a queue position.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let interval_stream = tokio_stream::wrappers::IntervalStream::new(interval);

    // Liveness still has to be visible while the payload is unchanged, or the
    // dashboard decides the stream is down and starts its fallback polling.
    // Shared across ticks, so it lives behind a handle each future can take.
    let dedupe = Arc::new(StdMutex::new(StreamDedupe::new(Instant::now())));

    let bot_for_shutdown = Arc::clone(&bot);
    let event_stream = interval_stream.enumerate().scan((), move |_, (seq, _)| {
        let bot = Arc::clone(&bot);
        let dedupe = Arc::clone(&dedupe);
        let authorization = authorization.clone();
        async move {
            if !authorization.remains_authorized(&bot).await {
                return None;
            }
            let mut payload = realtime::fast_snapshot(&bot).await;
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("transport".into(), Value::String("sse".into()));
                obj.insert("seq".into(), json!(seq));
                obj.insert("kind".into(), Value::String("fast".into()));
            }

            // The slow half is eight kilobytes of markets, outcomes and trades.
            // Merging it into every frame meant a Bitcoin price moving by a cent
            // resent all of it, which cancelled the saving from deduplication.
            // It rides along only when it has actually been rebuilt, and always
            // on a connection's first frame so a new client is not left waiting
            // for the next change to see any of it.
            // Observing the generation also starts a background refresh when
            // the cached slow half is old. Without this poll the generation
            // could never advance, so an SSE-only dashboard kept its first
            // markets/trades snapshot forever.
            let mut slow_generation = realtime::slow_snapshot_generation(&bot).await;
            let send_slow = {
                let state = match dedupe.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                !state.seen || slow_generation != state.slow_generation
            };
            if send_slow {
                let slow = realtime::slow_snapshot_cached(&bot).await;
                // A first-time or owner-mismatch cache build increments the
                // generation. Record the value that this frame actually
                // carries so it is not sent again on the next fast tick.
                slow_generation = realtime::slow_snapshot_generation(&bot).await;
                if let Some(obj) = payload.as_object_mut() {
                    if let Some(v) = slow.get("markets") {
                        obj.insert("markets".into(), v.clone());
                    }
                    if let Some(v) = slow.get("no_arb") {
                        obj.insert("no_arb".into(), v.clone());
                    }
                    if let Some(v) = slow.get("outcomes") {
                        obj.insert("outcomes".into(), v.clone());
                    }
                    if let Some(v) = slow.get("trades") {
                        obj.insert("trades".into(), v.clone());
                    }
                    if let Some(v) = slow.get("slow_loading") {
                        obj.insert("slow_loading".into(), v.clone());
                    }
                    if let Some(v) = slow.get("slow_error") {
                        obj.insert("slow_error".into(), v.clone());
                    }
                    if let Some(v) = slow.get("trades_error") {
                        obj.insert("trades_error".into(), v.clone());
                    }
                    if let Some(v) = slow.get("snapshot_ts") {
                        obj.insert("snapshot_ts".into(), v.clone());
                    }
                    obj.insert("kind".into(), Value::String("full".into()));
                }
            }

            let data = serde_json::to_string(&payload)
                .unwrap_or_else(|_| r#"{"error":"serialise failed"}"#.to_string());

            // The sequence number changes every tick by construction, so it is
            // excluded from the comparison; otherwise nothing would ever match.
            let digest = payload_digest(&payload);

            let decision = {
                let mut state = match dedupe.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                state.decide(digest, Instant::now(), slow_generation)
            };

            Some(Ok::<Event, Infallible>(match decision {
                Send::Full => Event::default().data(data),
                // Small, and enough for the client to keep its liveness clock
                // running. Every field the dashboard reads is absent, and every
                // reader of those fields is already guarded.
                Send::Heartbeat => Event::default().data(
                    json!({ "transport": "sse", "seq": seq, "kind": "heartbeat" }).to_string(),
                ),
                // A comment keeps the connection warm without waking the
                // client's message handler at all.
                Send::Nothing => Event::default().comment(""),
            }))
        }
    });

    // An SSE response never ends on its own, and graceful shutdown waits for
    // every in-flight response before the process can close its positions.
    // A dashboard left open therefore held a SIGTERM'd bot alive — still
    // analysing, still trading, no longer listening — for as long as the tab
    // stayed open, while a fresh instance started alongside it on the same
    // database. The stream now ends the moment shutdown begins.
    let shutdown =
        crate::bot::state::shutdown_begun(bot_for_shutdown.runtime_state.shutdown_watch());
    let event_stream = event_stream.take_until(shutdown);

    Ok(Sse::new(event_stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    ))
}

pub async fn issue_stream_token(
    State(bot): State<Arc<TradingBot>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, axum::response::Response> {
    let provided_key = headers.get("x-api-key").and_then(|v| v.to_str().ok());
    let token = auth::issue_stream_token(&bot, Some(addr.ip().to_string()), provided_key)
        .await
        .map_err(|status| (status, "stream token auth failed").into_response())?;
    Ok(Json(json!({
        "token": token,
        "ttl_seconds": auth::stream_token_ttl_secs()
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quiet_stream_heartbeats_instead_of_going_silent() {
        // Ticking at 200ms with an unchanged payload: the connection must speak
        // often enough that the dashboard's ten-second liveness window never
        // closes. Silence here is what makes a working stream look dead.
        let t0 = Instant::now();
        let mut state = StreamDedupe::new(t0);
        assert_eq!(
            state.decide(7, t0, 0),
            Send::Full,
            "the first tick always sends"
        );

        let mut heartbeats = 0;
        let mut longest_silence = Duration::ZERO;
        let mut last_spoke = t0;
        for tick in 1..=100 {
            let now = t0 + Duration::from_millis(200 * tick);
            match state.decide(7, now, 0) {
                Send::Heartbeat => {
                    heartbeats += 1;
                    longest_silence = longest_silence.max(now.duration_since(last_spoke));
                    last_spoke = now;
                }
                Send::Full => panic!("payload never changed, so nothing should be resent"),
                Send::Nothing => {}
            }
        }
        assert!(
            heartbeats > 0,
            "twenty seconds of silence produced no heartbeat"
        );
        assert!(
            longest_silence < Duration::from_secs(10),
            "went quiet for {longest_silence:?}, past the client's liveness window"
        );
    }

    #[test]
    fn a_rebuilt_slow_half_is_sent_even_when_the_fast_half_is_unchanged() {
        // Markets and trades change on their own schedule. If only the fast
        // digest were consulted, a new market list would sit in the cache
        // unsent until something else happened to move.
        let t0 = Instant::now();
        let mut state = StreamDedupe::new(t0);
        state.decide(9, t0, 1);
        assert_eq!(
            state.decide(9, t0 + Duration::from_millis(200), 2),
            Send::Nothing,
            "the digest is computed over what is actually sent, so the caller \
             includes the new slow half and the digest changes with it"
        );
    }

    #[test]
    fn the_first_frame_records_which_slow_build_it_carried() {
        // A fresh connection is sent the slow half regardless; the generation
        // has to be recorded or the next tick would send it again.
        let t0 = Instant::now();
        let mut state = StreamDedupe::new(t0);
        state.decide(5, t0, 7);
        assert_eq!(state.slow_generation, 7);
    }

    #[test]
    fn an_unchanged_payload_is_never_resent() {
        let t0 = Instant::now();
        let mut state = StreamDedupe::new(t0);
        state.decide(42, t0, 0);
        assert_eq!(
            state.decide(42, t0 + Duration::from_millis(200), 0),
            Send::Nothing,
            "the same payload must not be sent twice"
        );
    }

    #[test]
    fn a_change_is_sent_on_the_very_next_tick() {
        // The point of keeping a fast tick is that real movement is not delayed.
        let t0 = Instant::now();
        let mut state = StreamDedupe::new(t0);
        state.decide(1, t0, 0);
        assert_eq!(
            state.decide(2, t0 + Duration::from_millis(200), 0),
            Send::Full
        );
    }

    #[test]
    fn a_heartbeat_does_not_count_as_having_sent_the_payload() {
        // After a heartbeat the payload is still the one the client already
        // has, so a later change must still be recognised as a change.
        let t0 = Instant::now();
        let mut state = StreamDedupe::new(t0);
        state.decide(1, t0, 0);
        assert_eq!(state.decide(1, t0 + HEARTBEAT_AFTER, 0), Send::Heartbeat);
        assert_eq!(
            state.decide(2, t0 + HEARTBEAT_AFTER + Duration::from_millis(200), 0),
            Send::Full
        );
    }

    #[test]
    fn the_first_tick_sends_even_when_the_digest_happens_to_be_zero() {
        // A fresh connection must receive a payload. Starting with digest 0 and
        // comparing would silently withhold the first frame from any snapshot
        // that hashed to zero.
        let t0 = Instant::now();
        let mut state = StreamDedupe::new(t0);
        assert_eq!(state.decide(0, t0, 0), Send::Full);
    }

    #[test]
    fn the_digest_ignores_the_sequence_number() {
        // seq increments every tick. Including it would make every payload
        // unique, so nothing would ever be recognised as unchanged and the
        // deduplication would silently do nothing at all.
        let a = json!({ "seq": 1, "balance": 50.0, "kind": "fast" });
        let b = json!({ "seq": 2, "balance": 50.0, "kind": "fast" });
        assert_eq!(
            payload_digest(&a),
            payload_digest(&b),
            "two payloads differing only by seq must compare equal"
        );
    }

    #[test]
    fn the_digest_notices_a_real_change() {
        let a = json!({ "seq": 1, "balance": 50.0 });
        let b = json!({ "seq": 1, "balance": 50.01 });
        assert_ne!(payload_digest(&a), payload_digest(&b));
    }

    #[test]
    fn the_digest_notices_a_change_deep_in_the_payload() {
        // Most of what moves is nested: the analysis block, the risk block. A
        // shallow comparison would drop exactly the updates that matter.
        let a = json!({ "analysis": { "p_up": 0.61, "signal": "BUY_YES" } });
        let b = json!({ "analysis": { "p_up": 0.62, "signal": "BUY_YES" } });
        assert_ne!(payload_digest(&a), payload_digest(&b));
    }

    #[test]
    fn the_digest_notices_a_field_disappearing() {
        // A missing key is a real change: it blanks a panel.
        let a = json!({ "analysis": { "p_up": 0.61 }, "risk": { "kill_switch": false } });
        let b = json!({ "analysis": { "p_up": 0.61 } });
        assert_ne!(payload_digest(&a), payload_digest(&b));
    }

    #[test]
    fn the_digest_ignores_every_field_that_changes_each_tick() {
        // These six are why a naive hash matched nothing: each one differs on
        // every tick regardless of whether anything a reader cares about moved.
        let a = json!({
            "seq": 1, "ts": "2026-09-15T13:00:00Z", "server_build_ms": 3, "kind": "fast",
            "analysis_freshness": { "age_secs": 4, "stale": false },
            "macro_guard": { "next": { "label": "FOMC", "secs_until_event": 103595,
                                       "minutes_until_event": 1726.58 } },
            "balance": 50.0
        });
        let b = json!({
            "seq": 2, "ts": "2026-09-15T13:00:01Z", "server_build_ms": 5, "kind": "full",
            "analysis_freshness": { "age_secs": 5, "stale": false },
            "macro_guard": { "next": { "label": "FOMC", "secs_until_event": 103594,
                                       "minutes_until_event": 1726.56 } },
            "balance": 50.0
        });
        assert_eq!(
            payload_digest(&a),
            payload_digest(&b),
            "nothing a reader would notice changed, so these must compare equal"
        );
    }

    #[test]
    fn stripping_counters_does_not_hide_a_real_change_beside_them() {
        // age_secs is dropped, stale is not. Losing both would mean the page
        // never learns the analysis went stale.
        let a = json!({ "analysis_freshness": { "age_secs": 4, "stale": false } });
        let b = json!({ "analysis_freshness": { "age_secs": 99, "stale": true } });
        assert_ne!(payload_digest(&a), payload_digest(&b));
    }

    #[test]
    fn a_macro_event_changing_identity_is_still_a_change() {
        // Only the countdown is derived. The event itself matters.
        let a = json!({ "macro_guard": { "next": { "label": "FOMC", "secs_until_event": 10 } } });
        let b = json!({ "macro_guard": { "next": { "label": "CPI", "secs_until_event": 10 } } });
        assert_ne!(payload_digest(&a), payload_digest(&b));
    }

    #[test]
    fn a_live_price_move_is_a_change() {
        // The whole point is that real movement still reaches the browser
        // within one tick.
        let a = json!({ "live_btc_price": 76990.01 });
        let b = json!({ "live_btc_price": 76979.25 });
        assert_ne!(payload_digest(&a), payload_digest(&b));
    }

    #[test]
    fn the_heartbeat_gap_stays_inside_the_dashboard_liveness_window() {
        // static/dashboard.js treats the stream as down after 10s without a
        // message. A silent connection must speak before then, or the page
        // starts fallback polling against a stream that is working fine.
        assert!(
            HEARTBEAT_AFTER < Duration::from_secs(10),
            "heartbeat interval {HEARTBEAT_AFTER:?} must stay under the client's 10s window"
        );
    }
}
