//! Forwards unmatched Cursor RPCs to the original HTTPS host.

use std::time::Instant;

use axum::{
    body::{Body, Bytes},
    extract::Extension,
    http::{HeaderValue, Request, Response, header},
};

use crate::{Result, harness};

const CURSOR_UPSTREAM: &str = "https://api2.cursor.sh";
/// Cursor's upstream answers in well under this; a hung socket must not pin a
/// task for the app's whole lifetime.
const UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// How long the shared client keeps an idle connection to Cursor's API.
const UPSTREAM_POOL_IDLE: std::time::Duration = std::time::Duration::from_secs(90);

#[derive(Clone, Debug)]
pub struct CursorProxy {
    client: reqwest::Client,
    upstream: String,
}

/// One client for every forwarded call: timeouts on connect and response, and
/// pooled connections to the single upstream host. A builder failure is fatal
/// rather than silently falling back to a client with no timeouts.
fn upstream_client() -> reqwest::Client {
    const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    match reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(UPSTREAM_TIMEOUT)
        .pool_idle_timeout(UPSTREAM_POOL_IDLE)
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            // `Client::builder()` cannot fail on this platform with these
            // options, so the only sane answer is a client with the defaults.
            tracing::error!(%error, "cannot build the Cursor upstream client; using defaults");
            reqwest::Client::new()
        }
    }
}

impl Default for CursorProxy {
    fn default() -> Self {
        Self {
            client: upstream_client(),
            upstream: CURSOR_UPSTREAM.into(),
        }
    }
}

#[derive(Debug)]
pub struct BufferedResponse {
    pub status: axum::http::StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Bytes,
}

impl BufferedResponse {
    /// Turns the buffered reply into the streamed response axum sends.
    pub fn into_response(self) -> Response<Body> {
        let Self {
            status,
            headers,
            body,
        } = self;
        with_body(status, headers, body)
    }

    /// Replaces the buffered body while keeping the upstream status and headers.
    pub fn with_body(self, body: Bytes) -> Response<Body> {
        let Self {
            status, headers, ..
        } = self;
        with_body(status, headers, body)
    }
}

fn with_body(
    status: axum::http::StatusCode,
    mut headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response<Body> {
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// Axum handler for the forward-only paths.
pub async fn forward(
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    forward_request(&proxy, request).await
}

/// The client request's parts that reach the upstream as-is: method, resolved
/// URL, and headers with the harness field and every hop-by-hop field removed.
struct PreparedUpstream {
    method: axum::http::Method,
    url: String,
    headers: axum::http::HeaderMap,
    path: String,
}

impl PreparedUpstream {
    fn new(proxy: &CursorProxy, request: Request<Body>) -> Result<(Self, Body)> {
        let (parts, body) = request.into_parts();
        let path = parts
            .uri
            .path_and_query()
            .map_or("/", |value| value.as_str())
            .to_owned();
        let url = upstream_url(&parts.headers, &proxy.upstream, &path)?;
        let mut headers = parts.headers;
        headers.remove(harness::UPSTREAM_URL_HEADER);
        headers.remove(header::HOST);
        remove_hop_by_hop_headers(&mut headers);
        Ok((
            Self {
                method: parts.method,
                url,
                headers,
                path,
            },
            body,
        ))
    }
}

/// Streams `request` to the upstream Cursor host, adding the configured timeouts
/// and dropping hop-by-hop headers on both legs.
pub async fn forward_request(
    proxy: &CursorProxy,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let started = Instant::now();
    let (prepared, body) = PreparedUpstream::new(proxy, request)?;
    let mut upstream = match proxy
        .client
        .request(prepared.method.clone(), prepared.url)
        .headers(prepared.headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(
                method = %prepared.method,
                path = prepared.path,
                elapsed_ms = started.elapsed().as_millis(),
                %error,
                "Cursor upstream request failed"
            );
            return Err(error.into());
        }
    };
    let status = upstream.status();
    let mut response_headers = std::mem::take(upstream.headers_mut());
    remove_hop_by_hop_headers(&mut response_headers);
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    tracing::info!(
        method = %prepared.method,
        path = prepared.path,
        %status,
        elapsed_ms = started.elapsed().as_millis(),
        "forwarded Cursor backend request"
    );
    Ok(response)
}

/// Forwards `request` and buffers the whole reply, for handlers that have to patch the body.
pub async fn forward_buffered(
    proxy: &CursorProxy,
    request: Request<Body>,
) -> Result<BufferedResponse> {
    let (prepared, body) = PreparedUpstream::new(proxy, request)?;
    let PreparedUpstream {
        method,
        url,
        mut headers,
        ..
    } = prepared;
    // A body this side rewrites must be read uncompressed.
    headers.insert(
        "connect-accept-encoding",
        axum::http::HeaderValue::from_static("identity"),
    );
    headers.insert(
        header::ACCEPT_ENCODING,
        axum::http::HeaderValue::from_static("identity"),
    );
    let body = super::read_body(body).await?;
    let mut upstream = proxy
        .client
        .request(method, url)
        .headers(headers)
        .body(body)
        .send()
        .await?;
    let status = upstream.status();
    let mut headers = std::mem::take(upstream.headers_mut());
    remove_hop_by_hop_headers(&mut headers);
    let body = upstream.bytes().await?;
    Ok(BufferedResponse {
        status,
        headers,
        body,
    })
}

fn upstream_url(headers: &axum::http::HeaderMap, fallback: &str, path: &str) -> Result<String> {
    let Some(value) = headers.get(harness::UPSTREAM_URL_HEADER) else {
        return Ok(format!("{fallback}{path}"));
    };
    let value = value
        .to_str()
        .map_err(|error| crate::Error::Protocol(format!("invalid upstream URL header: {error}")))?;
    let url = reqwest::Url::parse(value)
        .map_err(|error| crate::Error::Protocol(format!("invalid upstream URL: {error}")))?;
    if url.scheme() != "https" || !url.host_str().is_some_and(harness::is_cursor_host) {
        return Err(crate::Error::Protocol(
            "upstream URL must target a Cursor HTTPS host".into(),
        ));
    }
    Ok(url.into())
}

fn remove_hop_by_hop_headers(headers: &mut axum::http::HeaderMap) {
    // A `Connection` header names the fields that are hop-by-hop for this leg
    // only; they are as removable as the fixed set below.
    let connection_headers: Vec<String> = headers
        .get(header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    for name in connection_headers {
        headers.remove(name);
    }
    for name in [
        header::CONNECTION,
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ] {
        headers.remove(name);
    }
    headers.remove("keep-alive");
}
