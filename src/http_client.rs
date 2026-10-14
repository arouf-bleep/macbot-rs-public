//! Shared reqwest::Client — a single connection-pooled HTTP client used
//! throughout the application.  Avoids creating multiple independent pools.

use once_cell::sync::Lazy;
use reqwest::Client;
use serde::de::DeserializeOwned;

pub const MAX_JSON_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub trait JsonResponseExt {
    fn bounded_json<T: DeserializeOwned>(
        self,
    ) -> impl std::future::Future<Output = anyhow::Result<T>> + Send;
}

impl JsonResponseExt for reqwest::Response {
    fn bounded_json<T: DeserializeOwned>(
        self,
    ) -> impl std::future::Future<Output = anyhow::Result<T>> + Send {
        read_json(self)
    }
}

/// reqwest's `Response::json` collects an unlimited body before decoding it.
/// A deadline does not bound how many bytes a fast or faulty provider sends.
pub async fn read_json<T: DeserializeOwned>(response: reqwest::Response) -> anyhow::Result<T> {
    read_json_limited(response, MAX_JSON_RESPONSE_BYTES).await
}

pub async fn read_json_limited<T: DeserializeOwned>(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> anyhow::Result<T> {
    if response.status().is_redirection() {
        anyhow::bail!("HTTP JSON request refused a redirect");
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        anyhow::bail!("HTTP JSON response exceeds the {max_bytes}-byte limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(reqwest::Error::without_url)?
    {
        if chunk.len() > max_bytes.saturating_sub(body.len()) {
            anyhow::bail!("HTTP JSON response exceeds the {max_bytes}-byte limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&body)?)
}

pub static HTTP: Lazy<Client> = Lazy::new(|| {
    Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("macbot-rs/1.0")
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .build()
        .expect("failed to build HTTP client with the required request timeout")
});

/// Authenticated venue requests must stay at the exact signed URL. reqwest's
/// default redirect policy strips standard Authorization headers across hosts,
/// but does not recognize the venue's custom POLY_* credentials. Redirecting
/// also changes the route/body covered by the original request signature.
pub static AUTHENTICATED_HTTP: Lazy<Client> = Lazy::new(|| {
    Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("macbot-rs/1.0")
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .build()
        .expect("failed to build non-redirecting authenticated HTTP client")
});

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn truncated_response_errors_never_expose_url_query_credentials() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut buffer = [0; 1024];
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0, "request ended before its headers");
                request.extend_from_slice(&buffer[..read]);
                assert!(request.len() < 8192);
            }
            assert!(
                String::from_utf8_lossy(&request).contains("api_key=public-body-secret-fixture")
            );
            // Announce more bytes than arrive, so the failure comes from the
            // response stream after successful request/header processing.
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\nConnection: close\r\n\r\n{\"partial\":",
                )
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        });
        let response = HTTP
            .get(format!("http://{address}/truncated"))
            .query(&[("api_key", "public-body-secret-fixture")])
            .send()
            .await
            .unwrap();
        assert_eq!(response.content_length(), Some(64));
        let error = read_json::<serde_json::Value>(response).await.unwrap_err();
        let transport_error = error
            .downcast_ref::<reqwest::Error>()
            .expect("the fixture must fail while reading its response body");
        assert!(transport_error.url().is_none());
        assert!(!format!("{error:#}").contains("public-body-secret-fixture"));
        assert!(!format!("{error:?}").contains("public-body-secret-fixture"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn provider_redirects_never_forward_query_secrets_headers_or_post_bodies() {
        use axum::{
            body::Body,
            extract::{Request, State},
            http::{header::LOCATION, StatusCode},
            response::Response,
            Router,
        };
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        #[derive(Clone)]
        struct RedirectFixture {
            destination: String,
            received: Arc<AtomicUsize>,
        }
        async fn redirect(State(fixture): State<RedirectFixture>, req: Request) -> Response {
            assert_eq!(req.method(), reqwest::Method::POST);
            assert_eq!(req.uri().query(), Some("api_key=public-query-fixture"));
            assert_eq!(req.headers()["x-api-key"], "public-header-fixture");
            assert!(!req.headers().contains_key("referer"));
            let code: u16 = req.uri().path().trim_start_matches('/').parse().unwrap();
            let body = axum::body::to_bytes(req.into_body(), 1024).await.unwrap();
            assert_eq!(body.as_ref(), br#"{"private":"public-body-fixture"}"#);
            fixture.received.fetch_add(1, Ordering::SeqCst);
            Response::builder()
                .status(StatusCode::from_u16(code).unwrap())
                .header(LOCATION, &fixture.destination)
                // A parseable body must not turn a redirect into a successful result.
                .body(Body::from(r#"{"redirect":true}"#))
                .unwrap()
        }
        async fn destination(State(received): State<Arc<AtomicUsize>>) -> &'static str {
            received.fetch_add(1, Ordering::SeqCst);
            r#"{"forwarded":true}"#
        }

        let forwarded = Arc::new(AtomicUsize::new(0));
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target.local_addr().unwrap();
        let target_counter = forwarded.clone();
        let target_task = tokio::spawn(async move {
            axum::serve(
                target,
                Router::new()
                    .fallback(destination)
                    .with_state(target_counter),
            )
            .await
            .unwrap();
        });
        let received = Arc::new(AtomicUsize::new(0));
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_address = origin.local_addr().unwrap();
        let fixture = RedirectFixture {
            destination: format!("http://{target_address}/destination"),
            received: received.clone(),
        };
        let origin_task = tokio::spawn(async move {
            axum::serve(origin, Router::new().fallback(redirect).with_state(fixture))
                .await
                .unwrap();
        });

        for code in [301, 302, 307, 308] {
            let response = HTTP
                .post(format!("http://{origin_address}/{code}"))
                .query(&[("api_key", "public-query-fixture")])
                .header("x-api-key", "public-header-fixture")
                .body(r#"{"private":"public-body-fixture"}"#)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), code);
            let error = read_json::<serde_json::Value>(response).await.unwrap_err();
            assert!(error.to_string().contains("refused a redirect"));
        }
        assert_eq!(received.load(Ordering::SeqCst), 4);
        assert_eq!(forwarded.load(Ordering::SeqCst), 0);
        origin_task.abort();
        target_task.abort();
    }

    #[tokio::test]
    async fn bounded_json_checks_actual_stream_and_accepts_only_valid_json_within_limit() {
        use axum::{body::Body, extract::Request, response::Response, Router};
        use serde_json::Value;

        async fn serve(req: Request) -> Response {
            let body = match req.uri().path() {
                "/declared" => Body::from(r#""123456789""#),
                "/chunked" => Body::from_stream(futures_util::stream::iter([
                    Ok::<_, std::io::Error>(r#""12345"#),
                    Ok(r#"6789""#),
                ])),
                "/invalid" => Body::from("{bad}"),
                _ => panic!("unexpected fixture route"),
            };
            Response::new(body)
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, Router::new().fallback(serve))
                .await
                .unwrap();
        });
        for path in ["declared", "chunked"] {
            for limit in [10, 11] {
                let response = HTTP
                    .get(format!("http://{address}/{path}"))
                    .send()
                    .await
                    .unwrap();
                if path == "declared" {
                    assert_eq!(response.content_length(), Some(11));
                } else {
                    assert_eq!(response.content_length(), None);
                }
                let result = read_json_limited::<Value>(response, limit).await;
                if limit == 10 {
                    assert!(result.unwrap_err().to_string().contains("byte limit"));
                } else {
                    assert_eq!(result.unwrap(), serde_json::json!("123456789"));
                }
            }
        }
        let response = HTTP
            .get(format!("http://{address}/invalid"))
            .send()
            .await
            .unwrap();
        assert!(read_json_limited::<Value>(response, 11).await.is_err());
        task.abort();
    }
}
