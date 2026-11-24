//! Behavioral coverage for capture decoding, persistence, and the debug API.

use std::convert::Infallible;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use flate2::Compression;
use flate2::write::GzEncoder;
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;

use crate::Server;
use crate::config::{Config, validate_loopback_address};
use crate::decode::{
    ConnectFrameDecoder, decode_captured_content, decode_unary_request, snake_case,
    streaming_request_message_type, streaming_response_message_type,
};
use crate::store::ExchangeStore;
use crate::types::{Exchange, ExchangeState, ExchangeSummary, Payload};

#[test]
fn loopback_address_is_required() {
    assert!(validate_loopback_address("127.0.0.1:9090").is_ok());
    assert!(validate_loopback_address("localhost:9090").is_ok());
    assert!(validate_loopback_address("0.0.0.0:9090").is_err());
    assert!(validate_loopback_address("192.168.1.8:9090").is_err());
}

#[test]
fn snake_case_matches_upstream() {
    assert_eq!(
        snake_case("AvailableModelsRequest"),
        "available_models_request"
    );
    assert_eq!(snake_case("BidiAppend"), "bidi_append");
}

#[test]
fn run_sse_uses_hardcoded_stream_types() {
    assert_eq!(
        streaming_request_message_type("/agent.v1.AgentService/RunSSE").as_deref(),
        Some("aiserver.v1.BidiRequestId")
    );
    assert_eq!(
        streaming_response_message_type("/agent.v1.AgentService/RunSSE").as_deref(),
        Some("agent.v1.AgentServerMessage")
    );
}

#[test]
fn connect_frames_decode_across_chunk_boundaries() {
    let json = br#"{"a":1}"#;
    let mut payload = vec![
        0x00,
        0x00,
        0x00,
        0x00,
        u8::try_from(json.len()).expect("short json"),
    ];
    payload.extend_from_slice(json);
    payload.extend_from_slice(&[0x02, 0x00, 0x00, 0x00, 0x02, b'{', b'}']);
    let frames = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let collected = std::sync::Arc::clone(&frames);
    let mut decoder = ConnectFrameDecoder::new("unknown.Message", "", 8, move |frame| {
        collected.lock().unwrap().push(frame);
    });
    decoder.write(&payload[..3]);
    decoder.write(&payload[3..8]);
    decoder.write(&payload[8..]);
    decoder.close();
    let frames = frames.lock().unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].length, 7);
    assert!(
        frames[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("未知的 protobuf"))
    );
    assert!(frames[1].end_stream);
    assert_eq!(frames[1].kind, "end_stream");
    assert!(frames[1].json.contains('{'));
}

#[test]
fn gzip_json_content_is_pretty_printed() {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(br#"{"ok":true}"#).unwrap();
    let compressed = encoder.finish().unwrap();
    let decoded = decode_captured_content(&compressed, "application/json", "gzip");
    assert!(decoded.error.is_none());
    assert_eq!(decoded.language, "json");
    assert!(decoded.text.contains("\"ok\": true"));
}

#[test]
fn bidi_append_hex_client_message_extracts_conversation_id() {
    let client = encode_agent_run_request("conv-1");
    let client_hex = hex::encode(&client);
    let request = encode_bidi_append(&client_hex, b"");
    let decoded = decode_unary_request("/aiserver.v1.BidiService/BidiAppend", &request);
    assert_eq!(decoded.kind, "run_request");
    assert_eq!(decoded.request_id, "req-9");
    assert_eq!(decoded.conversation_id, "conv-1");
    assert!(decoded.json.contains("agent_client_message"));
}

fn encode_agent_run_request(conversation_id: &str) -> Vec<u8> {
    let mut inner = Vec::new();
    inner.push(0x2a);
    inner.extend(encode_len(conversation_id.len()));
    inner.extend(conversation_id.as_bytes());
    let mut message = Vec::new();
    message.push(0x0a);
    message.extend(encode_len(inner.len()));
    message.extend(inner);
    message
}

fn encode_bidi_append(data: &str, data_binary: &[u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    if !data.is_empty() {
        payload.push(0x0a);
        payload.extend(encode_len(data.len()));
        payload.extend(data.as_bytes());
    }
    let mut request_id = Vec::new();
    request_id.push(0x0a);
    request_id.extend(encode_len("req-9".len()));
    request_id.extend(b"req-9");
    payload.push(0x12);
    payload.extend(encode_len(request_id.len()));
    payload.extend(request_id);
    if !data_binary.is_empty() {
        payload.push(0x22);
        payload.extend(encode_len(data_binary.len()));
        payload.extend(data_binary);
    }
    payload
}

fn encode_len(len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut value = u64::try_from(len).expect("protobuf lengths fit in u64");
    while value >= 0x80 {
        out.push(u8::try_from(value & 0x7f).expect("low 7 bits") | 0x80);
        value >>= 7;
    }
    out.push(u8::try_from(value).expect("final byte is below 0x80"));
    out
}

fn sample_exchange(id: &str, conversation_id: &str, request_id: &str) -> Exchange {
    Exchange {
        summary: ExchangeSummary {
            id: id.into(),
            started_at: Utc::now(),
            method: "POST".into(),
            url: "https://api2.cursor.sh/agent.v1.AgentService/RunSSE".into(),
            host: "api2.cursor.sh".into(),
            path: "/agent.v1.AgentService/RunSSE".into(),
            status: Some(200),
            state: ExchangeState::Completed,
            duration_ms: 12,
            request_bytes: 4,
            response_bytes: 8,
            request_id: request_id.into(),
            conversation_id: conversation_id.into(),
            request_kind: "run_request".into(),
            response_kind: "text".into(),
            frame_count: 1,
            error: None,
        },
        request: Payload::default(),
        response: Payload::default(),
    }
}

#[test]
fn store_associates_conversation_by_request_id() {
    let dir = TempDir::new().unwrap();
    let store = ExchangeStore::persistent(dir.path().join("capture.db"), 20).unwrap();
    let _ = store.create(sample_exchange("1", "conv-a", "req-a"));
    let id = store.create(sample_exchange("2", "", ""));
    store.update(&id, |exchange| {
        exchange.summary.request_id = "req-a".into();
    });
    let two = store.get("2").unwrap().unwrap();
    assert_eq!(two.summary.conversation_id, "conv-a");
    let conversations = store.conversations().unwrap();
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].exchange_count, 2);
}

#[test]
fn an_update_closure_that_reaches_back_into_the_store_is_refused_not_hung() {
    let store = ExchangeStore::memory(20);
    let id = store.create(sample_exchange("1", "conv-a", "req-a"));

    let read_back = std::cell::Cell::new(None);
    // Without the guard this would block forever on the non-reentrant lock the
    // update already holds.
    store.update(&id, |exchange| {
        exchange.summary.status = Some(418);
        read_back.set(store.get("1").unwrap());
    });

    assert!(
        read_back.take().is_none(),
        "the reentrant read is refused rather than deadlocking"
    );
    let status = store.status();
    assert!(
        status
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("refused")),
        "{status:?}"
    );
    assert_eq!(store.get("1").unwrap().unwrap().summary.status, Some(418));
}

#[tokio::test]
async fn debug_api_serves_original_ui_and_status() {
    let dir = TempDir::new().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route(
                "/echo",
                axum::routing::post(|body: String| async move { body }),
            ),
        )
        .await
        .unwrap();
    });

    let capture_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = capture_listener.local_addr().unwrap();
    let server = Arc::new(
        Server::new(Config {
            service_addr: addr.to_string(),
            upstream_url: upstream,
            database_path: dir.path().join("capture.db"),
            ..Config::default()
        })
        .unwrap(),
    );
    let app = Arc::clone(&server).router();

    let ui = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/__debuger__/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ui.status(), StatusCode::OK);
    let html =
        String::from_utf8(ui.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(html.contains("Cursor 协议调试器"));

    let status = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/__debuger__/api/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let payload: Value =
        serde_json::from_slice(&status.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(payload["running"], true);
    assert_eq!(payload["debugPath"], "/__debuger__/");

    let proxied = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/echo")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"hello":"cursor"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(proxied.status(), StatusCode::OK);
    assert_eq!(
        proxied.into_body().collect().await.unwrap().to_bytes(),
        r#"{"hello":"cursor"}"#
    );
    // The finished body is written by the capture sink, not the forwarding path.
    server.flush_captures();

    let list = app
        .oneshot(
            Request::builder()
                .uri("/__debuger__/api/exchanges")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let exchanges: Vec<Value> =
        serde_json::from_slice(&list.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(exchanges.len(), 1);
    assert_eq!(exchanges[0]["path"], "/echo");
    assert_eq!(exchanges[0]["method"], "POST");
    assert_eq!(exchanges[0]["state"], "completed");
}

#[tokio::test]
async fn stream_completes_and_persists_both_chunks() {
    let dir = TempDir::new().unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    let server = serve_stream_fixture(&dir, std::sync::Arc::clone(&gate)).await;
    let body = first_stream_chunk(Arc::clone(&server).router()).await;
    assert_eq!(
        server.store().get("1").unwrap().unwrap().summary.state,
        ExchangeState::Streaming
    );

    gate.notify_one();
    let tail = tokio::time::timeout(Duration::from_secs(5), body.collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    assert_eq!(tail, "second");
    // The last body is written by the capture sink; wait for it before dropping.
    server.flush_captures();
    drop(server);

    let reopened = ExchangeStore::persistent(dir.path().join("stream.db"), 20).unwrap();
    let exchange = reopened.get("1").unwrap().unwrap();
    assert_eq!(exchange.summary.state, ExchangeState::Completed);
    assert_eq!(exchange.summary.response_bytes, 11);
    assert_eq!(exchange.response.raw_hex, hex::encode("firstsecond"));
}

#[tokio::test]
async fn clear_during_stream_does_not_overwrite_reused_id() {
    let dir = TempDir::new().unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    let server = serve_stream_fixture(&dir, std::sync::Arc::clone(&gate)).await;
    let app = Arc::clone(&server).router();
    let old_body = first_stream_chunk(app.clone()).await;
    assert_eq!(
        server.store().get("1").unwrap().unwrap().summary.state,
        ExchangeState::Streaming
    );

    let clear = app
        .clone()
        .oneshot(
            Request::delete("/__debuger__/api/exchanges")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(clear.status(), StatusCode::NO_CONTENT);
    assert!(server.store().get("1").unwrap().is_none());
    let response = app
        .oneshot(Request::get("/new").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "new"
    );

    gate.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), old_body.collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes(),
        "second"
    );
    // The old body's tail and the new request are both captured off the
    // forwarding path; let the sink finish before reading the store.
    server.flush_captures();
    for exchange in [
        server.store().get("1").unwrap().unwrap(),
        ExchangeStore::persistent(dir.path().join("stream.db"), 20)
            .unwrap()
            .get("1")
            .unwrap()
            .unwrap(),
    ] {
        assert_eq!(exchange.summary.path, "/new");
        assert_eq!(exchange.summary.state, ExchangeState::Completed);
        assert_eq!(exchange.response.raw_hex, hex::encode("new"));
    }
}

#[tokio::test]
async fn dropping_partial_stream_records_an_error_but_empty_body_completes() {
    let dir = TempDir::new().unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    let server = serve_stream_fixture(&dir, gate).await;
    let app = Arc::clone(&server).router();
    drop(first_stream_chunk(app.clone()).await);
    // Capture work runs off the forwarding path; wait for it before asserting.
    server.flush_captures();

    let exchange = server.store().get("1").unwrap().unwrap();
    assert_eq!(exchange.summary.state, ExchangeState::Error);
    assert!(
        exchange
            .summary
            .error
            .as_deref()
            .is_some_and(|error| error.contains("取消"))
    );
    assert_eq!(exchange.response.raw_hex, hex::encode("first"));

    let response = app
        .oneshot(Request::get("/empty").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    drop(response);
    server.flush_captures();
    assert_eq!(
        server.store().get("2").unwrap().unwrap().summary.state,
        ExchangeState::Completed
    );
}

async fn serve_stream_fixture(dir: &TempDir, gate: Arc<tokio::sync::Notify>) -> Arc<Server> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    let upstream = axum::Router::new()
        .route(
            "/stream",
            axum::routing::get(move || async move {
                Body::from_stream(async_stream::stream! {
                    yield Ok::<_, Infallible>(bytes::Bytes::from_static(b"first"));
                    gate.notified().await;
                    yield Ok::<_, Infallible>(bytes::Bytes::from_static(b"second"));
                })
            }),
        )
        .route("/new", axum::routing::get(|| async { "new" }))
        .route(
            "/empty",
            axum::routing::get(|| async { StatusCode::NO_CONTENT }),
        );
    tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    Arc::new(
        Server::new(Config {
            upstream_url,
            database_path: dir.path().join("stream.db"),
            ..Config::default()
        })
        .unwrap(),
    )
}

async fn first_stream_chunk(app: axum::Router) -> Body {
    tokio::time::timeout(Duration::from_secs(5), async {
        let response = app
            .oneshot(Request::get("/stream").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        let frame = body.frame().await.unwrap().unwrap();
        assert_eq!(frame.into_data().unwrap(), "first");
        body
    })
    .await
    .expect("first chunk must arrive before the upstream gate opens")
}
