//! Reverse-proxy Cursor API traffic while capturing a diagnostic copy.

use std::sync::{Arc, Mutex};
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
use url::Url;

use crate::capture::CaptureBody;
use crate::config::{Config, DEBUG_BASE_PATH, browser_address};
use crate::decode::{
    ConnectFrameDecoder, decode_captured_content, decode_unary_request, decode_unary_response,
    decodes_unary_request, decodes_unary_response, decompress_payload, is_unary_proto_content_type,
    raw_hex, streaming_request_message_type, streaming_response_message_type,
};
use crate::store::{CaptureId, ExchangeStore, sorted_headers};
use crate::types::{Error, Exchange, ExchangeSummary, Payload, Result};
use crate::web;

type HttpsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>;

#[derive(Clone)]
pub struct Server {
    config: Config,
    upstream: Url,
    store: ExchangeStore,
    client: HttpsClient,
}

impl Server {
    pub fn new(config: Config) -> Result<Arc<Self>> {
        let config = config.normalized()?;
        let upstream = Url::parse(&config.upstream_url)
            .map_err(|err| Error::Config(format!("解析固定上游地址：{err}")))?;
        let store = ExchangeStore::persistent(&config.database_path, config.max_exchanges)?;
        Ok(Arc::new(Self {
            config,
            upstream,
            store,
            client: https_client(),
        }))
    }

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

    pub fn service_addr(&self) -> &str {
        &self.config.service_addr
    }

    pub fn ui_url(&self) -> String {
        format!(
            "http://{}{DEBUG_BASE_PATH}/",
            browser_address(&self.config.service_addr)
        )
    }

    pub fn database_path(&self) -> String {
        self.config.database_path.display().to_string()
    }

    pub fn upstream_url(&self) -> String {
        self.upstream.as_str().trim_end_matches('/').to_string()
    }

    pub fn store(&self) -> &ExchangeStore {
        &self.store
    }

    pub fn running(&self) -> bool {
        true
    }

    pub fn clear_exchanges(&self) -> Result<()> {
        self.store.clear()
    }

    fn capture_request(&self, request: Request<Body>) -> (CaptureId, Request<Body>) {
        let path = request.uri().path().to_string();
        let mut upstream_url = self.upstream.clone();
        upstream_url.set_path(&path);
        upstream_url.set_query(request.uri().query());
        let request_content_type = header_value(request.headers(), "content-type");
        let request_codec = request_content_codec(&path, request.headers());
        let exchange = Exchange {
            summary: ExchangeSummary {
                id: String::new(),
                started_at: Utc::now(),
                method: request.method().as_str().to_string(),
                url: upstream_url.to_string(),
                host: self.upstream.host_str().unwrap_or_default().to_string(),
                path: path.clone(),
                status: 0,
                state: "pending".into(),
                duration_ms: 0,
                request_bytes: 0,
                response_bytes: 0,
                request_id: String::new(),
                conversation_id: String::new(),
                request_kind: String::new(),
                response_kind: String::new(),
                frame_count: 0,
                error: String::new(),
            },
            request: Payload {
                headers: sorted_headers(request.headers()),
                content_type: request_content_type.clone(),
                content_codec: request_codec.clone(),
                frames: Vec::new(),
                ..Payload::default()
            },
            response: Payload::default(),
        };
        let id = self.store.create(exchange);

        let (parts, body) = request.into_parts();
        let captured =
            self.wrap_request_body(body, id.clone(), path, request_content_type, request_codec);
        (id, Request::from_parts(parts, Body::new(captured)))
    }

    fn wrap_request_body(
        &self,
        body: Body,
        id: CaptureId,
        path: String,
        content_type: String,
        codec: String,
    ) -> CaptureBody<Body> {
        let max_frames = self.config.max_frames;
        let decoder = streaming_request_message_type(&path).map(|message_type| {
            let store = self.store.clone();
            let frame_id = id.clone();
            Arc::new(Mutex::new(ConnectFrameDecoder::new(
                message_type,
                codec.clone(),
                max_frames,
                move |frame| {
                    store.update_transient(&frame_id, |exchange| {
                        if !frame.kind.is_empty() {
                            exchange.summary.request_kind.clone_from(&frame.kind);
                        }
                        if !frame.request_id.is_empty() {
                            exchange.summary.request_id.clone_from(&frame.request_id);
                        }
                        if (exchange.request.frames.len() as i32) < max_frames {
                            exchange.request.frames.push(frame);
                        }
                    });
                },
            )))
        });
        let store = self.store.clone();
        let finish_id = id;
        let finish_path = path;
        let finish_type = content_type;
        let finish_codec = codec;
        let chunk_decoder = decoder.clone();
        CaptureBody::new(
            body,
            self.config.max_capture_bytes.max(0) as usize,
            move |chunk| {
                if let Some(decoder) = &chunk_decoder {
                    decoder.lock().expect("decoder").write(chunk);
                }
            },
            move |captured, size, truncated, end| {
                if let Some(decoder) = decoder {
                    decoder.lock().expect("decoder").close();
                }
                finish_request_body(
                    &store,
                    CapturedBody {
                        id: finish_id,
                        path: finish_path,
                        content_type: finish_type,
                        codec: finish_codec,
                        captured,
                        size,
                        truncated,
                        read_err: end.error(),
                    },
                );
            },
        )
    }

    fn wrap_response_body(
        &self,
        body: hyper::body::Incoming,
        id: CaptureId,
        path: String,
        content_type: String,
        codec: String,
    ) -> CaptureBody<hyper::body::Incoming> {
        let max_frames = self.config.max_frames;
        let decoder = streaming_response_message_type(&path).map(|message_type| {
            let store = self.store.clone();
            let frame_id = id.clone();
            Arc::new(Mutex::new(ConnectFrameDecoder::new(
                message_type,
                codec.clone(),
                max_frames,
                move |frame| {
                    store.update_transient(&frame_id, |exchange| {
                        if !frame.kind.is_empty() && frame.kind != "end_stream" {
                            exchange.summary.response_kind.clone_from(&frame.kind);
                        }
                        if !frame.error.is_empty() {
                            exchange.response.decode_error.clone_from(&frame.error);
                        }
                        if (exchange.response.frames.len() as i32) < max_frames {
                            exchange.response.frames.push(frame);
                        }
                        exchange.summary.frame_count = exchange.response.frames.len() as i32;
                    });
                },
            )))
        });
        let store = self.store.clone();
        let chunk_decoder = decoder.clone();
        CaptureBody::new(
            body,
            self.config.max_capture_bytes.max(0) as usize,
            move |chunk| {
                if let Some(decoder) = &chunk_decoder {
                    decoder.lock().expect("decoder").write(chunk);
                }
            },
            move |captured, size, truncated, end| {
                if let Some(decoder) = decoder {
                    decoder.lock().expect("decoder").close();
                }
                finish_response_body(
                    &store,
                    CapturedBody {
                        id,
                        path,
                        content_type,
                        codec,
                        captured,
                        size,
                        truncated,
                        read_err: end.error(),
                    },
                );
            },
        )
    }

    fn fail_exchange(&self, id: &CaptureId, error: impl std::fmt::Display) {
        self.store.update(id, |exchange| {
            exchange.summary.state = "error".into();
            exchange.summary.error = error.to_string();
            exchange.summary.duration_ms = elapsed_ms(exchange.summary.started_at);
        });
    }
}

fn https_client() -> HttpsClient {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    http.set_connect_timeout(Some(Duration::from_secs(10)));
    http.set_keepalive(Some(Duration::from_secs(30)));
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(http);
    Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(32)
        .build(https)
}

async fn proxy(State(server): State<Arc<Server>>, request: Request<Body>) -> Response<Body> {
    let path = request.uri().path().to_string();
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
            let content_type = header_value(&headers, "content-type");
            let codec = response_content_codec(&path, &headers);
            server.store.update(&id, |exchange| {
                exchange.summary.status = status.as_u16() as i32;
                exchange.summary.state = "streaming".into();
                exchange.summary.duration_ms = elapsed_ms(exchange.summary.started_at);
                exchange.response.headers = sorted_headers(&headers);
                exchange.response.content_type = content_type.clone();
                exchange.response.content_codec = codec.clone();
            });
            let captured =
                server.wrap_response_body(upstream.into_body(), id, path, content_type, codec);
            let mut response = Response::new(Body::new(captured));
            *response.status_mut() = status;
            *response.headers_mut() = headers;
            response
        }
        Err(err) => {
            server.fail_exchange(&id, err);
            gateway_error()
        }
    }
}

fn rewrite_upstream(upstream: &Url, request: Request<Body>) -> Result<Request<Body>> {
    let (mut parts, body) = request.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    parts.uri = outgoing_uri(upstream, path_and_query)?;
    strip_hop_by_hop(&mut parts.headers);
    parts.headers.remove("x-forwarded-for");
    if let Some(host) = upstream.host_str() {
        let host = if let Some(port) = upstream.port() {
            format!("{host}:{port}")
        } else {
            host.to_string()
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
    format!(
        "{}{}",
        base.as_str().trim_end_matches('/'),
        if path_and_query.starts_with('/') {
            path_and_query.to_string()
        } else {
            format!("/{path_and_query}")
        }
    )
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

fn request_content_codec(path: &str, headers: &HeaderMap) -> String {
    if streaming_request_message_type(path).is_some() {
        header_value(headers, "connect-content-encoding")
    } else {
        header_value(headers, "content-encoding")
    }
}

fn response_content_codec(path: &str, headers: &HeaderMap) -> String {
    if streaming_response_message_type(path).is_some() {
        return header_value(headers, "connect-content-encoding");
    }
    if !decodes_unary_response(path) {
        let codec = header_value(headers, "connect-content-encoding");
        if !codec.is_empty() {
            return codec;
        }
    }
    header_value(headers, "content-encoding")
}

struct CapturedBody {
    id: CaptureId,
    path: String,
    content_type: String,
    codec: String,
    captured: Vec<u8>,
    size: i64,
    truncated: bool,
    read_err: Option<String>,
}

fn finish_request_body(store: &ExchangeStore, body: CapturedBody) {
    let CapturedBody {
        id,
        path,
        content_type,
        codec,
        captured,
        size,
        truncated,
        read_err,
    } = body;
    let mut decode_payload = captured.clone();
    let mut content_decode_err = None;
    let decode_proto = decodes_unary_request(&path) && is_unary_proto_content_type(&content_type);
    if decode_proto && truncated {
        content_decode_err = Some("请求正文超过抓取上限，无法完整解码".into());
    } else if decode_proto && !codec.is_empty() && !codec.eq_ignore_ascii_case("identity") {
        match decompress_payload(&captured, &codec) {
            Ok(decoded) => decode_payload = decoded,
            Err(err) => content_decode_err = Some(err),
        }
    }
    let mut decoded_json = String::new();
    let mut decoded_lang = String::new();
    let mut kind = String::new();
    let mut request_id = String::new();
    let mut conversation_id = String::new();
    let mut decode_err = content_decode_err;
    if decode_proto && decode_err.is_none() {
        let decoded = decode_unary_request(&path, &decode_payload);
        decoded_json = decoded.json;
        kind = decoded.kind;
        request_id = decoded.request_id;
        conversation_id = decoded.conversation_id;
        decode_err = decoded.error;
    }
    if decode_err.is_none() && !decoded_json.is_empty() {
        decoded_lang = "json".into();
    } else if !decode_proto {
        let decoded = decode_captured_content(&captured, &content_type, &codec);
        decoded_json = decoded.text;
        decoded_lang = decoded.language;
        decode_err = decoded.error;
    }
    store.update(&id, |exchange| {
        exchange.summary.request_bytes = size;
        exchange.request.size = size;
        exchange.request.raw_hex = raw_hex(&captured);
        exchange.request.raw_truncated = truncated;
        if !decoded_json.is_empty() {
            exchange.request.decoded_json = decoded_json.clone();
            exchange.request.decoded_lang = decoded_lang.clone();
        }
        if !kind.is_empty() {
            exchange.summary.request_kind = kind.clone();
        }
        if !request_id.is_empty() {
            exchange.summary.request_id = request_id.clone();
        }
        if !conversation_id.is_empty() {
            exchange.summary.conversation_id = conversation_id.clone();
        }
        if let Some(err) = &decode_err {
            exchange.request.decode_error = err.clone();
        }
        if let Some(err) = read_err {
            exchange.summary.error = err;
        }
    });
}

fn finish_response_body(store: &ExchangeStore, body: CapturedBody) {
    let CapturedBody {
        id,
        path,
        content_type,
        codec,
        captured,
        size,
        truncated,
        read_err,
    } = body;
    let mut decode_payload = captured.clone();
    let mut content_decode_err = None;
    let decode_proto = decodes_unary_response(&path) && is_unary_proto_content_type(&content_type);
    if decode_proto && truncated {
        content_decode_err = Some("响应正文超过抓取上限，无法完整解码".into());
    } else if decode_proto && !codec.is_empty() && !codec.eq_ignore_ascii_case("identity") {
        match decompress_payload(&captured, &codec) {
            Ok(decoded) => decode_payload = decoded,
            Err(err) => content_decode_err = Some(err),
        }
    }
    let mut decoded_json = String::new();
    let mut decoded_lang = String::new();
    let mut kind = String::new();
    let mut decode_err = content_decode_err;
    if decode_proto && decode_err.is_none() {
        match decode_unary_response(&path, &decode_payload) {
            Ok((json, response_kind)) => {
                decoded_json = json;
                kind = response_kind;
            }
            Err(err) => decode_err = Some(err),
        }
    }
    if decode_err.is_none() && !decoded_json.is_empty() {
        decoded_lang = "json".into();
    } else if !decode_proto {
        let decoded = decode_captured_content(&captured, &content_type, &codec);
        decoded_json = decoded.text;
        decoded_lang = decoded.language;
        decode_err = decoded.error;
    }
    store.update(&id, |exchange| {
        exchange.summary.response_bytes = size;
        exchange.response.size = size;
        exchange.response.raw_hex = raw_hex(&captured);
        exchange.response.raw_truncated = truncated;
        if !decoded_json.is_empty() {
            exchange.response.decoded_json = decoded_json.clone();
            exchange.response.decoded_lang = decoded_lang.clone();
        }
        if !kind.is_empty() {
            exchange.summary.response_kind = kind.clone();
        }
        if let Some(err) = &decode_err {
            exchange.response.decode_error = err.clone();
        }
        exchange.summary.duration_ms = elapsed_ms(exchange.summary.started_at);
        exchange.summary.state = "completed".into();
        if let Some(err) = read_err {
            exchange.summary.state = "error".into();
            exchange.summary.error = err;
        }
    });
}

fn header_value(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn elapsed_ms(started_at: chrono::DateTime<Utc>) -> i64 {
    (Utc::now() - started_at).num_milliseconds().max(0)
}
