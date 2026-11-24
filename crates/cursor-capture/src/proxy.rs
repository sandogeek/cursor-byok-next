//! Reverse-proxy Cursor API traffic while capturing a diagnostic copy.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode, Uri};
use axum::routing::get;
use chrono::Utc;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use parking_lot::Mutex;
use url::Url;

use crate::capture::{BodyCompleteness, CaptureBody, CapturedBody};
use crate::config::{Config, DEBUG_BASE_PATH, browser_address};
use crate::decode::{
    ConnectFrameDecoder, capture_limit, clamped_i32, decode_captured_content, decode_unary_request,
    decode_unary_response, decodes_unary_request, decodes_unary_response, decompress_payload,
    is_unary_proto_content_type, raw_hex, streaming_request_message_type,
    streaming_response_message_type,
};
use crate::sink::CaptureSink;
use crate::store::{CaptureId, ExchangeStore, sorted_headers};
use crate::types::{Error, Exchange, ExchangeState, ExchangeSummary, FrameView, Payload, Result};
use crate::web;

type HttpsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>;

/// How long a TCP connect to Cursor's API may take before the request fails.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// TCP keepalive idle time, so a quiet SSE stream is not reaped by a NAT.
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);
/// How long an idle pooled upstream connection is reused.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Concurrent idle connections kept per upstream host.
const POOL_MAX_IDLE_PER_HOST: usize = 32;

#[derive(Clone, Debug)]
pub struct Server {
    config: Config,
    upstream: Url,
    /// `upstream` without its trailing slash, precomputed for reporting.
    upstream_url: String,
    database_path: String,
    store: ExchangeStore,
    /// Runs capture decode and persistence off the forwarding path.
    sink: CaptureSink,
    client: HttpsClient,
}

impl Server {
    /// Prepares the capture store and the HTTPS client.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when the listen address is not loopback or the
    /// upstream URL does not parse, and [`Error::Database`] when the store
    /// cannot open its SQLite file.
    pub fn new(config: Config) -> Result<Self> {
        let config = config.normalized()?;
        let upstream = Url::parse(&config.upstream_url)
            .map_err(|err| Error::Config(format!("解析固定上游地址：{err}")))?;
        let store = ExchangeStore::persistent(&config.database_path, config.max_exchanges)?;
        let upstream_url = upstream.as_str().trim_end_matches('/').to_owned();
        let database_path = config.database_path.display().to_string();
        Ok(Self {
            config,
            upstream,
            upstream_url,
            database_path,
            store,
            sink: CaptureSink::start(),
            client: https_client(),
        })
    }

    /// The debugger's own router; `Arc<Server>` is the axum state.
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route(
                DEBUG_BASE_PATH,
                get(|| async {
                    axum::response::Redirect::temporary(&format!("{DEBUG_BASE_PATH}/"))
                }),
            )
            .nest(&format!("{DEBUG_BASE_PATH}/"), web::router())
            .fallback(proxy)
            .with_state(self)
    }

    /// The loopback address the proxy listens on.
    pub fn service_addr(&self) -> &str {
        &self.config.service_addr
    }

    /// The URL a human opens to reach the debug UI.
    pub fn ui_url(&self) -> String {
        format!(
            "http://{}{DEBUG_BASE_PATH}/",
            browser_address(&self.config.service_addr)
        )
    }

    /// The SQLite file capturing this session.
    pub fn database_path(&self) -> &str {
        &self.database_path
    }

    /// The upstream every captured request is forwarded to.
    pub fn upstream_url(&self) -> &str {
        &self.upstream_url
    }

    /// The capture store, for the debug API.
    pub fn store(&self) -> &ExchangeStore {
        &self.store
    }

    /// Drops every captured exchange.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] when SQLite cannot delete the rows.
    pub fn clear_exchanges(&self) -> Result<()> {
        self.store.clear()
    }

    /// Waits until background capture work finishes. Tests call it to observe
    /// writes that the forwarding path queues rather than performs inline.
    #[cfg(test)]
    pub fn flush_captures(&self) {
        self.sink.flush();
    }

    fn capture_request(&self, request: Request<Body>) -> (CaptureId, Request<Body>) {
        let path = request.uri().path().to_owned();
        let mut upstream_url = self.upstream.clone();
        upstream_url.set_path(&path);
        upstream_url.set_query(request.uri().query());
        let request_content_type = header_value(request.headers(), "content-type").to_owned();
        let request_codec = request_content_codec(&path, request.headers());
        let exchange = Exchange {
            summary: ExchangeSummary {
                id: String::new(),
                started_at: Utc::now(),
                method: request.method().as_str().to_owned(),
                url: upstream_url.to_string(),
                host: self.upstream.host_str().unwrap_or_default().to_owned(),
                path: path.clone(),
                status: None,
                state: ExchangeState::Pending,
                duration_ms: 0,
                request_bytes: 0,
                response_bytes: 0,
                request_id: String::new(),
                conversation_id: String::new(),
                request_kind: String::new(),
                response_kind: String::new(),
                frame_count: 0,
                error: None,
            },
            request: Payload {
                headers: sorted_headers(request.headers()),
                content_type: request_content_type.clone(),
                content_codec: request_codec.clone(),
                ..Payload::default()
            },
            response: Payload::default(),
        };
        let id = self.store.create(exchange);

        let (parts, body) = request.into_parts();
        let target = BodyTarget {
            id: id.clone(),
            path,
            content_type: request_content_type,
            codec: request_codec,
            side: Side::Request,
        };
        let captured = self.wrap_body(body, target);
        (id, Request::from_parts(parts, Body::new(captured)))
    }

    fn wrap_body<B>(&self, body: B, target: BodyTarget) -> CaptureBody<B>
    where
        B: http_body::Body,
    {
        let sink = self.sink.clone();
        let done_sink = sink.clone();
        let max_frames = self.config.max_frames;
        // Decoding happens on the sink thread, never inside `poll_frame` or
        // `Drop`. The only work the forwarding path does is a bounded queue push.
        let decoder: Option<Arc<Mutex<ConnectFrameDecoder>>> = target
            .side
            .streaming_message_type(&target.path)
            .map(|message_type| {
                let store = self.store.clone();
                let frame_id = target.id.clone();
                let side = target.side;
                let sink = sink.clone();
                Arc::new(Mutex::new(ConnectFrameDecoder::new(
                    &message_type,
                    &target.codec,
                    max_frames,
                    move |frame| {
                        let job_store = store.clone();
                        let frame_id = frame_id.clone();
                        let record = move || {
                            job_store.update_transient(&frame_id, |exchange| {
                                side.record_frame(exchange, frame);
                            });
                        };
                        if !sink.submit(Box::new(record)) {
                            store.note_error("抓包队列已满，流式帧记录不完整");
                        }
                    },
                )))
            });
        let chunk_decoder = decoder.clone();
        let store = self.store.clone();
        CaptureBody::new(
            body,
            capture_limit(self.config.max_capture_bytes),
            move |chunk| {
                if let Some(decoder) = &chunk_decoder {
                    decoder.lock().write(chunk);
                }
            },
            move |captured| {
                // Decode, serialization and the SQLite write all run on the sink
                // thread; `Drop` only hands the job over.
                let job_store = store.clone();
                let job = move || {
                    if let Some(decoder) = decoder {
                        decoder.lock().close();
                    }
                    target.store(&job_store, captured);
                };
                if !done_sink.submit(Box::new(job)) {
                    store.note_error("抓包队列已满，正文未写入数据库");
                }
            },
        )
    }

    fn fail_exchange(&self, id: &CaptureId, error: &dyn std::fmt::Display) {
        let message = error.to_string();
        self.store.update(id, |exchange| {
            exchange.summary.state = ExchangeState::Error;
            exchange.summary.error = Some(message);
            exchange.summary.duration_ms = elapsed_ms(exchange.summary.started_at);
        });
    }
}

fn https_client() -> HttpsClient {
    // The workspace build links both `ring` and `aws-lc-rs`, so rustls can no
    // longer pick a process-wide provider on its own. This crate only enables
    // `ring` (through hyper-rustls), so make that the explicit choice.
    static PROVIDER: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    http.set_connect_timeout(Some(CONNECT_TIMEOUT));
    http.set_keepalive(Some(TCP_KEEPALIVE));
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(http);
    Client::builder(TokioExecutor::new())
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
        .build(https)
}

async fn proxy(State(server): State<Arc<Server>>, request: Request<Body>) -> Response<Body> {
    let path = request.uri().path().to_owned();
    let (id, captured) = server.capture_request(request);
    let outgoing = match rewrite_upstream(&server.upstream, captured) {
        Ok(request) => request,
        Err(err) => {
            server.fail_exchange(&id, &err);
            return gateway_error();
        }
    };
    match server.client.request(outgoing).await {
        Ok(upstream) => {
            let status = upstream.status();
            let mut headers = upstream.headers().clone();
            strip_hop_by_hop(&mut headers);
            let content_type = header_value(&headers, "content-type").to_owned();
            let codec = response_content_codec(&path, &headers);
            server.store.update(&id, |exchange| {
                exchange.summary.status = Some(status.as_u16());
                exchange.summary.state = ExchangeState::Streaming;
                exchange.summary.duration_ms = elapsed_ms(exchange.summary.started_at);
                exchange.response.headers = sorted_headers(&headers);
                exchange.response.content_type.clone_from(&content_type);
                exchange.response.content_codec.clone_from(&codec);
            });
            let captured = server.wrap_body(
                upstream.into_body(),
                BodyTarget {
                    id,
                    path,
                    content_type,
                    codec,
                    side: Side::Response,
                },
            );
            let mut response = Response::new(Body::new(captured));
            *response.status_mut() = status;
            *response.headers_mut() = headers;
            response
        }
        Err(err) => {
            server.fail_exchange(&id, &err);
            gateway_error()
        }
    }
}

fn rewrite_upstream(upstream: &Url, request: Request<Body>) -> Result<Request<Body>> {
    let (mut parts, body) = request.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map_or("/", http::uri::PathAndQuery::as_str);
    parts.uri = outgoing_uri(upstream, path_and_query)?;
    strip_hop_by_hop(&mut parts.headers);
    parts.headers.remove("x-forwarded-for");
    if let Some(host) = upstream.host_str() {
        let host = match upstream.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        };
        if let Ok(value) = HeaderValue::from_str(&host) {
            parts.headers.insert(http::header::HOST, value);
        }
    }
    Ok(Request::from_parts(parts, body))
}

fn outgoing_uri(upstream: &Url, path_and_query: &str) -> Result<Uri> {
    let mut base = upstream.clone();
    base.set_path("");
    base.set_query(None);
    let suffix = if path_and_query.starts_with('/') {
        path_and_query.to_owned()
    } else {
        format!("/{path_and_query}")
    };
    format!("{}{suffix}", base.as_str().trim_end_matches('/'))
        .parse()
        .map_err(|err| Error::Config(format!("构造上游地址失败：{err}")))
}

fn gateway_error() -> Response<Body> {
    let mut response = Response::new(Body::from("Cursor API upstream unavailable\n"));
    *response.status_mut() = StatusCode::BAD_GATEWAY;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let extra: Vec<HeaderName> = headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|item| item.trim().parse().ok())
        .collect();
    for name in extra {
        headers.remove(name);
    }
    for name in [
        http::header::CONNECTION,
        http::header::PROXY_AUTHENTICATE,
        http::header::PROXY_AUTHORIZATION,
        http::header::TE,
        http::header::TRAILER,
        http::header::TRANSFER_ENCODING,
        http::header::UPGRADE,
        HeaderName::from_static("keep-alive"),
    ] {
        headers.remove(name);
    }
}

/// Which half of the exchange a captured body belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Request,
    Response,
}

impl Side {
    /// The streaming decoder message type for this side's RPC.
    fn streaming_message_type(self, path: &str) -> Option<String> {
        match self {
            Self::Request => streaming_request_message_type(path),
            Self::Response => streaming_response_message_type(path),
        }
    }

    /// Whether a unary protobuf body on this side can be decoded.
    fn decodes_unary(self, path: &str) -> bool {
        match self {
            Self::Request => decodes_unary_request(path),
            Self::Response => decodes_unary_response(path),
        }
    }

    /// The label used in the "body too large to decode" message.
    fn label(self) -> &'static str {
        match self {
            Self::Request => "请求",
            Self::Response => "响应",
        }
    }

    fn payload(self, exchange: &mut Exchange) -> &mut Payload {
        match self {
            Self::Request => &mut exchange.request,
            Self::Response => &mut exchange.response,
        }
    }

    /// Records one decoded Connect frame into the summary and payload.
    fn record_frame(self, exchange: &mut Exchange, frame: FrameView) {
        let is_end_stream = frame.kind == FrameView::KIND_END_STREAM;
        match self {
            Self::Request => {
                if !frame.kind.is_empty() && !is_end_stream {
                    exchange.summary.request_kind.clone_from(&frame.kind);
                }
                if !frame.request_id.is_empty() {
                    exchange.summary.request_id.clone_from(&frame.request_id);
                }
                exchange.request.frames.push(frame);
            }
            Self::Response => {
                if !frame.kind.is_empty() && !is_end_stream {
                    exchange.summary.response_kind.clone_from(&frame.kind);
                }
                if let Some(error) = &frame.error {
                    exchange.response.decode_error = Some(error.clone());
                }
                exchange.response.frames.push(frame);
                exchange.summary.frame_count = clamped_i32(exchange.response.frames.len());
            }
        }
    }
}

/// What a finished body is decoded against, fixed when the body was wrapped.
struct BodyTarget {
    id: CaptureId,
    path: String,
    content_type: String,
    codec: String,
    side: Side,
}

/// One finished capture, ready to be written into the exchange.
#[derive(Debug)]
struct FinishedBody<'a> {
    size: i64,
    bytes: &'a [u8],
    completeness: BodyCompleteness,
    decode_error: Option<String>,
}

/// The decoded view of one captured body, as the debug UI shows it.
#[derive(Debug, Default)]
struct DecodedSide {
    json: String,
    language: String,
    kind: String,
    request_id: String,
    conversation_id: String,
}

impl BodyTarget {
    fn store(self, store: &ExchangeStore, captured: CapturedBody) {
        let CapturedBody {
            bytes,
            size,
            completeness,
            end,
        } = captured;
        let (decoded, decode_error) = self.decode(&bytes, completeness);
        let read_error = end.error();
        let finished = FinishedBody {
            size,
            bytes: &bytes,
            completeness,
            decode_error,
        };
        store.update(&self.id, |exchange| {
            exchange.publish_body(self.side, &decoded, &finished);
            if let Some(err) = read_error {
                exchange.summary.state = ExchangeState::Error;
                exchange.summary.error = Some(err);
            }
        });
    }

    /// Decodes one body, returning the view to show plus the reason it is
    /// incomplete when decoding failed.
    fn decode(
        &self,
        bytes: &[u8],
        completeness: BodyCompleteness,
    ) -> (DecodedSide, Option<String>) {
        let proto =
            self.side.decodes_unary(&self.path) && is_unary_proto_content_type(&self.content_type);
        if !proto {
            let content = decode_captured_content(bytes, &self.content_type, &self.codec);
            return (
                DecodedSide {
                    json: content.text,
                    language: content.language,
                    ..DecodedSide::default()
                },
                content.error,
            );
        }
        // A truncated protobuf body cannot be decoded at all, so say why
        // instead of reporting a parse failure on half a message.
        if completeness == BodyCompleteness::Truncated {
            return (
                DecodedSide::default(),
                Some(format!(
                    "{}正文超过抓取上限，无法完整解码",
                    self.side.label()
                )),
            );
        }
        let decompressed = if self.codec.is_empty() || self.codec.eq_ignore_ascii_case("identity") {
            None
        } else {
            match decompress_payload(bytes, &self.codec) {
                Ok(decoded) => Some(decoded),
                Err(err) => return (DecodedSide::default(), Some(err.to_string())),
            }
        };
        decode_protobuf(
            self.side,
            &self.path,
            decompressed.as_deref().unwrap_or(bytes),
        )
    }
}

fn decode_protobuf(side: Side, path: &str, payload: &[u8]) -> (DecodedSide, Option<String>) {
    match side {
        Side::Request => {
            let decoded = decode_unary_request(path, payload);
            (
                DecodedSide {
                    json: decoded.json,
                    kind: decoded.kind,
                    request_id: decoded.request_id,
                    conversation_id: decoded.conversation_id,
                    language: String::new(),
                },
                decoded.error,
            )
        }
        Side::Response => match decode_unary_response(path, payload) {
            Ok(decoded) => (
                DecodedSide {
                    json: decoded.json,
                    kind: decoded.kind,
                    ..DecodedSide::default()
                },
                None,
            ),
            Err(err) => (DecodedSide::default(), Some(err.to_string())),
        },
    }
}

impl Exchange {
    /// Writes one finished body into the summary and payload.
    fn publish_body(&mut self, side: Side, decoded: &DecodedSide, finished: &FinishedBody<'_>) {
        let raw_hex = raw_hex(finished.bytes);
        match side {
            Side::Request => {
                self.summary.request_bytes = finished.size;
                if !decoded.kind.is_empty() {
                    self.summary.request_kind.clone_from(&decoded.kind);
                }
                if !decoded.request_id.is_empty() {
                    self.summary.request_id.clone_from(&decoded.request_id);
                }
                if !decoded.conversation_id.is_empty() {
                    self.summary
                        .conversation_id
                        .clone_from(&decoded.conversation_id);
                }
            }
            Side::Response => {
                self.summary.response_bytes = finished.size;
                if !decoded.kind.is_empty() {
                    self.summary.response_kind.clone_from(&decoded.kind);
                }
                self.summary.duration_ms = elapsed_ms(self.summary.started_at);
                if self.summary.state != ExchangeState::Error {
                    self.summary.state = ExchangeState::Completed;
                }
            }
        }
        let is_json = !decoded.json.is_empty();
        let language = if decoded.language.is_empty() && is_json {
            "json"
        } else {
            decoded.language.as_str()
        };
        let payload = side.payload(self);
        payload.size = finished.size;
        payload.raw_hex = raw_hex;
        payload.raw_truncated = finished.completeness == BodyCompleteness::Truncated;
        if is_json {
            payload.decoded_json.clone_from(&decoded.json);
            language.clone_into(&mut payload.decoded_lang);
        }
        if let Some(err) = &finished.decode_error {
            payload.decode_error = Some(err.clone());
        }
    }
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .unwrap_or_default()
}

fn request_content_codec(path: &str, headers: &HeaderMap) -> String {
    let name = if streaming_request_message_type(path).is_some() {
        "connect-content-encoding"
    } else {
        "content-encoding"
    };
    header_value(headers, name).to_owned()
}

fn response_content_codec(path: &str, headers: &HeaderMap) -> String {
    if streaming_response_message_type(path).is_some() {
        return header_value(headers, "connect-content-encoding").to_owned();
    }
    if !decodes_unary_response(path) {
        let codec = header_value(headers, "connect-content-encoding");
        if !codec.is_empty() {
            return codec.to_owned();
        }
    }
    header_value(headers, "content-encoding").to_owned()
}

fn elapsed_ms(started_at: chrono::DateTime<Utc>) -> i64 {
    (Utc::now() - started_at).num_milliseconds().max(0)
}
