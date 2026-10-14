//! Lightweight Binance WebSocket price bus for BTC/ETH/SOL mids.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use once_cell::sync::Lazy;
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::{info, warn};

const BINANCE_PRICE_STREAM_URL: &str =
    "wss://stream.binance.com:9443/stream?streams=btcusdt@bookTicker/ethusdt@bookTicker/solusdt@bookTicker";
const HISTORY_RETENTION: Duration = Duration::from_secs(5 * 60);
const SUPPORTED_PRICE_SYMBOLS: [&str; 3] = ["BTCUSDT", "ETHUSDT", "SOLUSDT"];
// The consumers ask for at most a 70-second lookback. A generous point
// ceiling keeps normal high-rate feeds intact while bounding a burst or a
// faulty provider; missing older history continues to return None.
const MAX_HISTORY_POINTS: usize = 100_000;
/// Binance's bookTicker fires many times a second for BTC, so this much
/// silence means the connection is dead, whether or not the socket says so.
const STALL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy)]
struct PricePoint {
    ts: Instant,
    price: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PriceBusSnapshot {
    pub price: f64,
    pub age_ms: u64,
}

#[derive(Debug, Default)]
struct SymbolState {
    history: VecDeque<PricePoint>,
}

impl SymbolState {
    fn record(&mut self, price: f64) {
        let point = PricePoint {
            ts: Instant::now(),
            price,
        };
        self.history.push_back(point);
        let cutoff = point.ts.checked_sub(HISTORY_RETENTION).unwrap_or(point.ts);
        while let Some(front) = self.history.front() {
            if front.ts < cutoff {
                self.history.pop_front();
            } else {
                break;
            }
        }
        while self.history.len() > MAX_HISTORY_POINTS {
            self.history.pop_front();
        }
    }

    fn latest(&self) -> Option<PricePoint> {
        self.history.back().copied()
    }

    fn snapshot_near(&self, target_age_secs: f64, tolerance_secs: f64) -> Option<(f64, f64)> {
        let now = Instant::now();
        let mut best: Option<(f64, f64)> = None;
        let mut best_delta = f64::INFINITY;
        for point in self.history.iter().rev() {
            let age_secs = now.duration_since(point.ts).as_secs_f64();
            if age_secs + tolerance_secs < target_age_secs {
                continue;
            }
            if age_secs > target_age_secs + tolerance_secs {
                break;
            }
            let delta = (age_secs - target_age_secs).abs();
            if delta <= tolerance_secs && delta < best_delta {
                best = Some((point.price, age_secs));
                best_delta = delta;
            }
        }
        best
    }
}

#[derive(Default)]
pub struct PriceBus {
    symbols: RwLock<HashMap<String, SymbolState>>,
}

impl PriceBus {
    pub fn new() -> Self {
        let mut symbols = HashMap::new();
        for symbol in SUPPORTED_PRICE_SYMBOLS {
            symbols.insert(symbol.to_string(), SymbolState::default());
        }
        Self {
            symbols: RwLock::new(symbols),
        }
    }

    pub async fn record_mid(&self, symbol: &str, price: f64) {
        if !SUPPORTED_PRICE_SYMBOLS.contains(&symbol) || price <= 0.0 || !price.is_finite() {
            return;
        }
        let mut guard = self.symbols.write().await;
        guard.entry(symbol.to_string()).or_default().record(price);
    }

    pub async fn latest_price(&self, symbol: &str, max_age_secs: f64) -> Option<f64> {
        let snapshot = self.symbol_snapshot(symbol).await?;
        if snapshot.age_ms as f64 <= max_age_secs * 1000.0 {
            Some(snapshot.price)
        } else {
            None
        }
    }

    pub async fn symbol_snapshot(&self, symbol: &str) -> Option<PriceBusSnapshot> {
        let guard = self.symbols.read().await;
        let state = guard.get(symbol)?;
        let latest = state.latest()?;
        if latest.ts.elapsed().as_secs_f64() > 5.0 {
            return None;
        }
        let age_ms = latest.ts.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        Some(PriceBusSnapshot {
            price: latest.price,
            age_ms,
        })
    }

    pub async fn snapshot_near(
        &self,
        symbol: &str,
        target_age_secs: f64,
        tolerance_secs: f64,
    ) -> Option<(f64, f64)> {
        let guard = self.symbols.read().await;
        guard
            .get(symbol)
            .and_then(|state| state.snapshot_near(target_age_secs, tolerance_secs))
    }

    pub async fn price_change_pct(
        &self,
        symbol: &str,
        lookback_secs: f64,
        tolerance_secs: f64,
    ) -> Option<f64> {
        let guard = self.symbols.read().await;
        let state = guard.get(symbol)?;
        let latest = state.latest()?;
        let (then_price, _) = state.snapshot_near(lookback_secs, tolerance_secs)?;
        if then_price <= 0.0 {
            return None;
        }
        Some((latest.price - then_price) / then_price * 100.0)
    }
}

static SHARED_PRICE_BUS: Lazy<Arc<PriceBus>> = Lazy::new(|| Arc::new(PriceBus::new()));

pub fn shared_price_bus() -> Arc<PriceBus> {
    Arc::clone(&SHARED_PRICE_BUS)
}

fn parse_price(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn parse_book_ticker_mid(text: &str) -> Option<(String, f64)> {
    let parsed: Value = serde_json::from_str(text).ok()?;
    let data = parsed.get("data").unwrap_or(&parsed);
    let symbol = data.get("s").and_then(|v| v.as_str())?.to_string();
    let bid = data.get("b").and_then(parse_price).unwrap_or(0.0);
    let ask = data.get("a").and_then(parse_price).unwrap_or(0.0);
    let price = if bid > 0.0 && ask > 0.0 {
        (bid + ask) / 2.0
    } else if bid > 0.0 {
        bid
    } else {
        ask
    };
    if price > 0.0 {
        Some((symbol, price))
    } else {
        None
    }
}

/// Why a price bus session stopped reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionEnd {
    /// The peer closed, or the stream ran out.
    Closed,
    /// The socket reported an error.
    ReadError,
    /// Nothing arrived for `STALL_TIMEOUT`.
    Stalled,
}

impl std::fmt::Display for SessionEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SessionEnd::Closed => "stream closed",
            SessionEnd::ReadError => "read error",
            SessionEnd::Stalled => "stalled: no message within the stall timeout",
        })
    }
}

/// Drain one connected session until it ends, and say why.
///
/// A plain `while let Some(msg) = read.next().await` only exits on a Close
/// frame, an error, or end-of-stream. A connection that dies silently — a
/// wifi blip, a NAT timeout, an interface drop where no FIN or RST ever
/// arrives — produces none of those, so the read waits forever and the
/// reconnect loop around it never runs. That is how the price bus sat dead
/// for thirteen minutes after a forty-second network outage while the L2
/// feed, which has this guard, recovered on its own. Generic over the stream
/// so the stall can be simulated.
async fn run_session<S>(bus: &PriceBus, read: &mut S, stall_after: Duration) -> SessionEnd
where
    S: futures_util::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use tokio_tungstenite::tungstenite::Message;
    loop {
        match tokio::time::timeout(stall_after, read.next()).await {
            Err(_elapsed) => return SessionEnd::Stalled,
            Ok(None) => return SessionEnd::Closed,
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Some((symbol, price)) = parse_book_ticker_mid(&text) {
                    bus.record_mid(&symbol, price).await;
                }
            }
            Ok(Some(Ok(Message::Close(_)))) => return SessionEnd::Closed,
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => {
                warn!("Price bus read error: {e}");
                return SessionEnd::ReadError;
            }
        }
    }
}

pub fn spawn_price_bus() {
    let bus = shared_price_bus();
    crate::task_monitor::spawn_monitored_task("price_bus", async move {
        let mut backoff_secs = 1u64;
        loop {
            info!("Price bus: connecting to Binance bookTicker stream");
            match crate::tls::connect_feed(BINANCE_PRICE_STREAM_URL).await {
                Ok((ws_stream, _)) => {
                    info!("Price bus: connected");
                    backoff_secs = 1;
                    // The write half stays bound for the life of the session so
                    // the library can answer Binance's pings through it.
                    let (_write, mut read) = ws_stream.split();
                    let end = run_session(&bus, &mut read, STALL_TIMEOUT).await;
                    warn!("Price bus: session ended: {end}");
                }
                Err(e) => {
                    warn!("Price bus connect error: {e}");
                }
            }
            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(30);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unexpected_feed_symbols_cannot_grow_the_price_bus() {
        use tokio_tungstenite::tungstenite::Message;

        let bus = PriceBus::new();
        let mut frames = Vec::new();
        for index in 0..100 {
            frames.push(Ok(Message::Text(
                serde_json::json!({"s":format!("UNSUBSCRIBED{index}"),"b":"100","a":"102"})
                    .to_string(),
            )));
        }
        frames.push(Ok(Message::Text(
            r#"{"s":"BTCUSDT","b":"100","a":"102"}"#.into(),
        )));
        frames.push(Ok(Message::Close(None)));
        let mut stream = Box::pin(futures_util::stream::iter(frames));
        assert_eq!(
            run_session(&bus, &mut stream, Duration::from_secs(1)).await,
            SessionEnd::Closed
        );
        assert_eq!(bus.symbols.read().await.len(), 3);
        assert_eq!(bus.latest_price("BTCUSDT", 5.0).await, Some(101.0));
        assert!(bus.symbol_snapshot("UNSUBSCRIBED0").await.is_none());
    }

    #[test]
    fn excessive_feed_rates_keep_only_bounded_newest_history() {
        let mut state = SymbolState::default();
        for index in 0..MAX_HISTORY_POINTS + 16 {
            state.record(index as f64 + 1.0);
        }
        assert_eq!(state.history.len(), MAX_HISTORY_POINTS);
        assert_eq!(state.history.front().unwrap().price, 17.0);
        assert_eq!(
            state.latest().unwrap().price,
            (MAX_HISTORY_POINTS + 16) as f64
        );
        assert!(state.snapshot_near(70.0, 1.0).is_none());
        assert!(state.snapshot_near(0.0, 1.0).is_some());
    }

    #[test]
    fn parse_book_ticker_mid_handles_combined_stream_payload() {
        let payload =
            r#"{"stream":"btcusdt@bookTicker","data":{"s":"BTCUSDT","b":"85000.0","a":"85010.0"}}"#;
        assert_eq!(
            parse_book_ticker_mid(payload),
            Some(("BTCUSDT".to_string(), 85005.0))
        );
    }

    #[test]
    fn lookback_search_skips_newer_ticks_to_find_the_historical_price() {
        let now = Instant::now();
        let state = SymbolState {
            history: VecDeque::from([
                PricePoint {
                    ts: now - Duration::from_secs(90),
                    price: 90.0,
                },
                PricePoint {
                    ts: now - Duration::from_secs(45),
                    price: 100.0,
                },
                PricePoint {
                    ts: now,
                    price: 110.0,
                },
            ]),
        };
        let (price, age) = state.snapshot_near(45.0, 5.0).expect("45-second tick");
        assert_eq!(price, 100.0);
        assert!((45.0..50.0).contains(&age));
        assert!(state.snapshot_near(20.0, 5.0).is_none());
    }

    /// A stream that delivers one tick and then goes silent, the way a
    /// connection that died without a FIN does.
    fn one_tick_then_silence() -> impl futures_util::Stream<
        Item = Result<
            tokio_tungstenite::tungstenite::Message,
            tokio_tungstenite::tungstenite::Error,
        >,
    > + Unpin {
        use tokio_tungstenite::tungstenite::Message;
        let tick = Message::Text(
            r#"{"stream":"btcusdt@bookTicker","data":{"s":"BTCUSDT","b":"100.0","a":"102.0"}}"#
                .into(),
        );
        Box::pin(
            futures_util::stream::once(async { Ok(tick) }).chain(futures_util::stream::pending()),
        )
    }

    #[tokio::test]
    async fn a_silent_connection_ends_the_session_instead_of_hanging_it() {
        let bus = PriceBus::new();
        let mut read = one_tick_then_silence();

        let end = tokio::time::timeout(
            Duration::from_secs(2),
            run_session(&bus, &mut read, Duration::from_millis(50)),
        )
        .await
        .expect("the session hung on a silent connection — the reconnect loop can never run");

        assert_eq!(end, SessionEnd::Stalled);
        // The tick that did arrive was not lost on the way to the stall.
        assert_eq!(bus.latest_price("BTCUSDT", 5.0).await, Some(101.0));
    }

    #[tokio::test]
    async fn a_close_frame_still_ends_the_session_cleanly() {
        use tokio_tungstenite::tungstenite::Message;
        let bus = PriceBus::new();
        let mut read = Box::pin(futures_util::stream::iter([Ok(Message::Close(None))]));
        let end = run_session(&bus, &mut read, Duration::from_secs(5)).await;
        assert_eq!(end, SessionEnd::Closed);
    }

    #[tokio::test]
    async fn price_bus_tracks_latest_and_lookback_change() {
        let bus = PriceBus::new();
        bus.record_mid("BTCUSDT", 100.0).await;
        tokio::time::sleep(Duration::from_millis(25)).await;
        bus.record_mid("BTCUSDT", 110.0).await;

        let latest = bus.latest_price("BTCUSDT", 1.0).await;
        assert_eq!(latest, Some(110.0));

        let snapshot = bus.symbol_snapshot("BTCUSDT").await;
        assert!(snapshot.is_some());
        assert_eq!(snapshot.unwrap().price, 110.0);

        let change = bus.price_change_pct("BTCUSDT", 0.02, 0.05).await;
        assert!(change.is_some());
        assert!(change.unwrap() > 0.0);
    }
}
