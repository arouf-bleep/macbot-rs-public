//! Process-wide TLS backend selection.
//!
//! rustls 0.23 will not choose a crypto provider for you when more than one is
//! compiled in, and this binary compiles in two: `aws-lc-rs` and `ring` both
//! arrive through reqwest, sqlx and tokio-tungstenite. Left unresolved, the
//! first TLS handshake on any task panics.
//!
//! That is not a cosmetic failure. The `l2_ingestion`, `price_bus` and
//! `liquidation_feed` tasks each open a WebSocket shortly after startup, so all
//! three died about fourteen seconds in; `l2_ingestion` is registered as
//! critical, so the task monitor engaged the kill switch. The bot halted itself
//! on every run with an order book that had never been populated, and nothing
//! reported that the halt and the panic were the same event.

/// Install the process-wide TLS provider.
///
/// Call once, before anything opens a socket. Returns whether this call was the
/// one that installed it; a second call is harmless and returns false.
///
/// Verifies afterwards and logs loudly if no provider ended up available,
/// because the alternative is discovering it from three panicking tasks and an
/// engaged kill switch fourteen seconds later.
pub fn install_provider() -> bool {
    let installed = rustls::crypto::ring::default_provider()
        .install_default()
        .is_ok();
    if !provider_installed() {
        tracing::error!(
            "No TLS crypto provider is available. Every TLS connection will panic \
             on its first handshake, the market feed tasks will die and the kill \
             switch will engage."
        );
    }
    installed
}

/// Whether a provider is available for the next handshake.
pub fn provider_installed() -> bool {
    rustls::crypto::CryptoProvider::get_default().is_some()
}

const FEED_MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const FEED_MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;

type FeedConnection = (
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::handshake::client::Response,
);

/// Bound TCP, TLS and WebSocket handshakes together. Read-loop deadlines only
/// apply after connecting, and do not protect a stalled handshake. The size
/// ceilings also keep provider frames below the library's broad defaults.
pub async fn connect_feed(
    url: &str,
) -> Result<FeedConnection, tokio_tungstenite::tungstenite::Error> {
    connect_feed_with_timeout(url, std::time::Duration::from_secs(10)).await
}

async fn connect_feed_with_timeout(
    url: &str,
    deadline: std::time::Duration,
) -> Result<FeedConnection, tokio_tungstenite::tungstenite::Error> {
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
        max_message_size: Some(FEED_MAX_MESSAGE_BYTES),
        max_frame_size: Some(FEED_MAX_FRAME_BYTES),
        max_write_buffer_size: 512 * 1024,
        ..Default::default()
    };
    tokio::time::timeout(
        deadline,
        tokio_tungstenite::connect_async_with_config(url, Some(config), false),
    )
    .await
    .map_err(|_| {
        tokio_tungstenite::tungstenite::Error::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "market-feed connection handshake timed out",
        ))
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stalled_websocket_handshake_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            connect_feed_with_timeout(
                &format!("ws://{address}"),
                std::time::Duration::from_millis(100),
            )
            .await
        });
        // Hold an accepted TCP connection without answering its WS handshake.
        let (_stalled_socket, _) = listener.accept().await.unwrap();
        let error = tokio::time::timeout(std::time::Duration::from_secs(2), client)
            .await
            .expect("the handshake deadline must release a stalled feed")
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            error,
            tokio_tungstenite::tungstenite::Error::Io(ref error)
                if error.kind() == std::io::ErrorKind::TimedOut
        ));
    }

    #[tokio::test]
    async fn feed_limits_reject_oversized_frames_and_fragmented_messages() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::{
            protocol::frame::{
                coding::{Data, OpCode},
                Frame,
            },
            Error, Message,
        };

        for fragmented in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
                websocket
                    .send(Message::Text("normal feed update".into()))
                    .await
                    .unwrap();
                if fragmented {
                    for index in 0..=4 {
                        let opcode = if index == 0 {
                            Data::Binary
                        } else {
                            Data::Continue
                        };
                        let size = if index == 4 { 1 } else { FEED_MAX_FRAME_BYTES };
                        let frame = Frame::message(vec![0; size], OpCode::Data(opcode), index == 4);
                        if websocket.send(Message::Frame(frame)).await.is_err() {
                            break;
                        }
                    }
                } else {
                    let _ = websocket
                        .send(Message::Binary(vec![0; FEED_MAX_FRAME_BYTES + 1]))
                        .await;
                }
            });
            let (mut websocket, _) = connect_feed(&format!("ws://{address}")).await.unwrap();
            assert_eq!(
                websocket.next().await.unwrap().unwrap(),
                Message::Text("normal feed update".into())
            );
            let rejected =
                tokio::time::timeout(std::time::Duration::from_secs(2), websocket.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap_err();
            assert!(matches!(rejected, Error::Capacity(_)));
            // Release the socket after refusal; the peer may still be writing
            // the payload that exceeded the limit.
            drop(websocket);
            tokio::time::timeout(std::time::Duration::from_secs(2), server)
                .await
                .expect("the rejected feed socket must release its peer")
                .unwrap();
        }
    }

    #[test]
    fn installing_leaves_a_provider_available_for_tls() {
        // Without this the first handshake panics, three background tasks die
        // and the kill switch engages. The whole point is that a provider is
        // present before any socket is opened.
        install_provider();
        assert!(
            provider_installed(),
            "no TLS provider is installed, so every TLS connection will panic \
             on its first handshake and the critical feed tasks will die"
        );
    }

    #[test]
    fn installing_twice_is_harmless() {
        // main calls this once, but a test binary or an embedder may have
        // installed one already. A second call must not panic or abort.
        install_provider();
        let second = install_provider();
        assert!(
            !second,
            "the second install should report that it did nothing"
        );
        assert!(provider_installed());
    }
}
