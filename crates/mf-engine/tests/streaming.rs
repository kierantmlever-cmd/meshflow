//! Transport-level tests for the streaming path: real socket, real reqwest, real SSE framing.
//!
//! The unit tests in `provider::openai` cover the decoder in isolation. These cover the part the
//! decoder can't see — when the stream is allowed to *end*.

use std::{net::SocketAddr, time::Duration};

use futures::StreamExt;
use mf_engine::{
    proto::{ProviderId, StopReason, StreamEvent},
    provider::{AiProvider, ChatRequest, Message, ProviderConfig, ProviderKind, openai::OpenAi},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

/// Serve one canned SSE response, then hold the connection open indefinitely — exactly what a
/// keep-alive provider does between requests.
async fn serve_then_hold(body: &'static str) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();

        // Drain the request head; we don't care what it says.
        let mut buf = [0u8; 4096];
        let _ = sock.read(&mut buf).await.unwrap();

        // No Content-Length and no Transfer-Encoding: the body runs until the socket closes.
        sock.write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{body}").as_bytes(),
        )
        .await
        .unwrap();
        sock.flush().await.unwrap();

        // Never close. If the client waits for EOF it waits forever.
        std::future::pending::<()>().await;
    });

    (addr, handle)
}

fn provider_at(addr: SocketAddr) -> OpenAi {
    OpenAi::new(
        ProviderConfig {
            id: ProviderId::new(),
            name: "test".into(),
            kind: ProviderKind::OpenAi,
            base_url: format!("http://{addr}/v1"),
            api_key: None,
            org_id: None,
            headers: Default::default(),
        },
        reqwest::Client::new(),
    )
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "test-model".into(),
        messages: vec![Message::user("hi")],
        ..Default::default()
    }
}

#[tokio::test]
async fn done_sentinel_terminates_the_stream() {
    const BODY: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n",
        "data: [DONE]\n\n",
    );

    let (addr, server) = serve_then_hold(BODY).await;
    let stream = provider_at(addr).stream(request()).await.expect("request should succeed");

    let events = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .expect("stream must end at [DONE] rather than waiting for the socket to close");
    server.abort();

    let events: Vec<StreamEvent> = events.into_iter().map(|e| e.expect("no stream error")).collect();
    assert_eq!(
        events,
        vec![
            StreamEvent::TextDelta("Hello".into()),
            StreamEvent::Done(StopReason::EndTurn),
            StreamEvent::Usage(mf_engine::proto::Usage { input_tokens: 3, output_tokens: 1 }),
        ]
    );
}

#[tokio::test]
async fn malformed_chunk_ends_the_stream_with_an_error() {
    const BODY: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
        "data: {not json\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"never seen\"}}]}\n\n",
    );

    let (addr, server) = serve_then_hold(BODY).await;
    let stream = provider_at(addr).stream(request()).await.expect("request should succeed");

    let events = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .expect("a decode error must terminate the stream, not stall it");
    server.abort();

    assert_eq!(events.len(), 2, "one delta, then the error, then nothing");
    assert_eq!(events[0].as_ref().unwrap(), &StreamEvent::TextDelta("ok".into()));
    assert!(events[1].is_err(), "malformed chunk must surface as an error");
}

#[tokio::test]
async fn http_error_status_is_reported_with_the_provider_message() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = sock.read(&mut buf).await.unwrap();
        let body = r#"{"error":{"message":"Incorrect API key provided"}}"#;
        sock.write_all(
            format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        sock.flush().await.unwrap();
    });

    // `BoxStream` isn't Debug, so unwrap the Result by hand rather than via expect_err.
    let err = match provider_at(addr).stream(request()).await {
        Err(e) => e,
        Ok(_) => panic!("401 must be an error, not a stream"),
    };
    server.abort();

    let rendered = err.to_string();
    assert!(rendered.contains("401"), "status should be visible: {rendered}");
    assert!(
        rendered.contains("Incorrect API key provided"),
        "the provider's own message is the useful part: {rendered}"
    );
}
