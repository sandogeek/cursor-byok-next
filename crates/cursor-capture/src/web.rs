//! Read-only debug API, SSE updates, and the original embedded web UI.

use std::convert::Infallible;
use std::sync::Arc;
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

use crate::config::DEBUG_BASE_PATH;
use crate::proxy::Server;

#[derive(Embed)]
#[folder = "web"]
struct WebAssets;

pub fn router() -> Router<Arc<Server>> {
    Router::new()
        .route("/", get(static_file))
        .route("/api/status", get(handle_status))
        .route("/api/exchanges", get(handle_exchange_list).delete(handle_clear_exchanges))
        .route("/api/exchanges/{id}", get(handle_exchange_detail))
        .route("/api/conversations", get(handle_conversation_list))
        .route("/api/events", get(handle_events))
        .fallback(static_file)
        .layer(middleware::from_fn(security_headers))
}

async fn handle_status(State(server): State<Arc<Server>>) -> impl IntoResponse {
    let (database_path, database_error) = server.store().status();
    axum::Json(serde_json::json!({
        "serviceAddr": server.service_addr(),
        "debugPath": format!("{DEBUG_BASE_PATH}/"),
        "upstreamURL": server.upstream_url(),
        "running": server.running(),
        "databasePath": database_path,
        "databaseError": database_error,
    }))
}

#[derive(Deserialize)]
struct ExchangeQuery {
    conversation_id: Option<String>,
}

async fn handle_exchange_list(
    State(server): State<Arc<Server>>,
    Query(query): Query<ExchangeQuery>,
) -> Response {
    let conversation_id = query
        .conversation_id
        .unwrap_or_default()
        .trim()
        .to_string();
    match server.store().summaries(&conversation_id) {
        Ok(summaries) => axum::Json(summaries).into_response(),
        Err(err) => json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

async fn handle_exchange_detail(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    match server.store().get(id.trim()) {
        Ok(Some(exchange)) => axum::Json(exchange).into_response(),
        Ok(None) => json_error(StatusCode::NOT_FOUND, "请求记录不存在"),
        Err(err) => json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

async fn handle_clear_exchanges(State(server): State<Arc<Server>>) -> Response {
    match server.clear_exchanges() {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

async fn handle_conversation_list(State(server): State<Arc<Server>>) -> Response {
    match server.store().conversations() {
        Ok(conversations) => axum::Json(conversations).into_response(),
        Err(err) => json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

async fn handle_events(State(server): State<Arc<Server>>) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let rx = server.store().subscribe();
    let stream = async_stream::stream! {
        yield Ok(Event::default().event("ready").data("{}"));
        let mut updates = BroadcastStream::new(rx);
        let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                event = updates.next() => {
                    match event {
                        Some(Ok(event)) => {
                            let payload = serde_json::to_string(&event).unwrap_or_else(|_| "{}".into());
                            yield Ok(Event::default().event("update").data(payload));
                        }
                        Some(Err(_)) => {
                            yield Ok(Event::default().event("update").data(r#"{"type":"updated"}"#));
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
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("heartbeat"))
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

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    let mut response = axum::Json(serde_json::json!({ "error": message.into() })).into_response();
    *response.status_mut() = status;
    response
}
