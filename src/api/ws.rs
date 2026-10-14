use axum::{
    extract::{
        connect_info::ConnectInfo,
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use crate::bot::state::TradingBot;

use super::{auth, realtime};

pub async fn stream_ws(
    State(bot): State<Arc<TradingBot>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
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
        Err(status) => return (status, "stream auth failed").into_response(),
    };

    ws.on_upgrade(move |socket| ws_session(socket, bot, authorization))
        .into_response()
}

async fn ws_session(
    mut socket: WebSocket,
    bot: Arc<TradingBot>,
    authorization: auth::StreamAuthorization,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut seq: u64 = 0;
    let shutdown = crate::bot::state::shutdown_begun(bot.runtime_state.shutdown_watch());
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            // See the SSE handler: an open socket must not hold shutdown open.
            _ = &mut shutdown => {
                let _ = tokio::time::timeout(Duration::from_secs(1), socket.close()).await;
                return;
            }
            _ = interval.tick() => {
                if !authorization.remains_authorized(&bot).await {
                    return;
                }
                let mut payload = realtime::fast_snapshot(&bot).await;
                if let Some(obj) = payload.as_object_mut() {
                    obj.insert("transport".into(), Value::String("ws".into()));
                    obj.insert("seq".into(), json!(seq));
                    obj.insert("kind".into(), Value::String("fast".into()));
                }

                if seq.is_multiple_of(8) {
                    let slow = realtime::slow_snapshot_cached(&bot).await;
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
                        obj.insert("kind".into(), Value::String("fast+slow".into()));
                    }
                }

                seq = seq.wrapping_add(1);

                let data = serde_json::to_string(&payload)
                    .unwrap_or_else(|_| r#"{"error":"serialise failed"}"#.to_string());

                if !matches!(tokio::time::timeout(Duration::from_secs(2), socket.send(Message::Text(data))).await, Ok(Ok(()))) {
                    break;
                }
            }
            msg = socket.next() => {
                match msg {
                    Some(Ok(Message::Ping(payload))) => {
                        if !matches!(tokio::time::timeout(Duration::from_secs(2), socket.send(Message::Pong(payload))).await, Ok(Ok(()))) {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
        }
    }
}
