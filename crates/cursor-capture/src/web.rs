//! Read-only debug API, SSE updates, and the original embedded web UI.

use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::StreamExt;
use rust_embed::Embed;
use serde::Deserialize;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::config::DEBUG_BASE_PATH;
use crate::store::ExchangeStore;
use crate::types::StoreEvent;

/// How often a quiet SSE connection is nudged so proxies keep it open.
const SSE_HEARTBEAT: Duration = Duration::from_secs(15);

#[derive(Embed)]
#[folder = "web"]
struct WebAssets;

/// The read-only session view and store exposed to the debug UI.
#[derive(Clone, Debug)]
pub(crate) struct DebugState {
    pub(crate) service_addr: String,
    pub(crate) upstream_url: String,
    pub(crate) store: ExchangeStore,
}

pub(crate) fn router(state: DebugState) -> Router {
    Router::new()
        .route("/", get(static_file))
        .route("/api/status", get(handle_status))
        .route(
            "/api/exchanges",
            get(handle_exchange_list).delete(handle_clear_exchanges),
        )
        .route("/api/exchanges/{id}", get(handle_exchange_detail))
        .route("/api/conversations", get(handle_conversation_list))
        .route("/api/events", get(handle_events))
        .fallback(static_file)
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn handle_status(State(state): State<DebugState>) -> impl IntoResponse {
    let status = state.store.status();
    axum::Json(serde_json::json!({
        "serviceAddr": state.service_addr,
        "debugPath": format!("{DEBUG_BASE_PATH}/"),
        "upstreamURL": state.upstream_url,
        "running": true,
        "databasePath": status.database_path,
        "databaseError": status.last_error,
    }))
}

#[derive(Deserialize)]
struct ExchangeQuery {
    conversation_id: Option<String>,
}

async fn handle_exchange_list(
    State(state): State<DebugState>,
    Query(query): Query<ExchangeQuery>,
) -> Response {
    let conversation_id = query.conversation_id.unwrap_or_default();
    match state.store.summaries(conversation_id.trim()) {
        Ok(summaries) => axum::Json(summaries).into_response(),
        Err(err) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

async fn handle_exchange_detail(
    State(state): State<DebugState>,
    Path(id): Path<String>,
) -> Response {
    match state.store.get(id.trim()) {
        Ok(Some(exchange)) => axum::Json(exchange).into_response(),
        Ok(None) => json_error(StatusCode::NOT_FOUND, "请求记录不存在"),
        Err(err) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

async fn handle_clear_exchanges(State(state): State<DebugState>) -> Response {
    match state.store.clear() {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

async fn handle_conversation_list(State(state): State<DebugState>) -> Response {
    match state.store.conversations() {
        Ok(conversations) => axum::Json(conversations).into_response(),
        Err(err) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

async fn handle_events(
    State(state): State<DebugState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.store.subscribe();
    let stream = async_stream::stream! {
        yield Ok(Event::default().event("ready").data("{}"));
        let mut updates = BroadcastStream::new(rx);
        let mut heartbeat = tokio::time::interval(SSE_HEARTBEAT);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                event = updates.next() => {
                    match event {
                        Some(Ok(event)) => {
                            yield Ok(Event::default().event("update").data(event_payload(&event)));
                        }
                        // The browser is behind: tell it to refetch rather than
                        // replay the changes it missed.
                        Some(Err(BroadcastStreamRecvError::Lagged(_))) => {
                            yield Ok(Event::default()
                                .event("update")
                                .data(r#"{"type":"cleared"}"#));
                        }
                        None => break,
                    }
                }
                _ = heartbeat.tick() => {
                    yield Ok(Event::default().comment("heartbeat"));
                }
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::new().interval(SSE_HEARTBEAT).text("heartbeat"))
}

async fn static_file(request: Request<Body>) -> Response {
    let mut path = request.uri().path().trim_start_matches('/');
    if path.is_empty() {
        path = "index.html";
    }
    match WebAssets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .status(StatusCode::OK)
                .header(http::header::CONTENT_TYPE, mime.as_ref())
                .body(Body::from(file.data.into_owned()))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn security_headers(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        http::header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        http::header::HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        http::header::HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self' https://cdn.jsdelivr.net; style-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; font-src 'self' https://cdn.jsdelivr.net data:; connect-src 'self'; worker-src 'self' blob:",
        ),
    );
    response
}

fn json_error(status: StatusCode, message: &str) -> Response {
    let mut response = axum::Json(serde_json::json!({ "error": message })).into_response();
    *response.status_mut() = status;
    response
}

/// Serializes a store event for the SSE stream.
fn event_payload(event: &StoreEvent) -> String {
    let mut value = serde_json::json!({ "type": event.kind.label() });
    if let Some(id) = &event.id {
        value["id"] = serde_json::Value::String(id.clone());
    }
    value.to_string()
}
