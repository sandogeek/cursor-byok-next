//! Forwards unmatched Cursor RPCs to the original HTTPS host.

use std::time::Instant;

use axum::{
    body::{Body, Bytes, to_bytes},
    extract::Extension,
    http::{HeaderValue, Request, Response, header},
};

use crate::{Result, backend::MAX_BUFFERED_BYTES, harness};

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
/// pooled connections to the single upstream host.
fn upstream_client() -> reqwest::Client {
    const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(UPSTREAM_TIMEOUT)
        .pool_idle_timeout(UPSTREAM_POOL_IDLE)
        .build()
        .unwrap_or_default()
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
    pub fn into_response(self) -> Response<Body> {
        let Self {
            status,
            headers,
            body,
        } = self;
        with_body(status, headers, body)
    }

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

pub async fn forward(
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let started = Instant::now();
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
    let upstream = match proxy
        .client
        .request(parts.method.clone(), url)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(
                method = %parts.method,
                path,
                elapsed_ms = started.elapsed().as_millis(),
                %error,
                "Cursor upstream request failed"
            );
            return Err(error.into());
        }
    };
    let status = upstream.status();
    let mut response_headers = upstream.headers().clone();
    remove_hop_by_hop_headers(&mut response_headers);
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    tracing::info!(
        method = %parts.method,
        path,
        %status,
        elapsed_ms = started.elapsed().as_millis(),
        "forwarded Cursor backend request"
    );
    Ok(response)
}

pub async fn forward_buffered(
    proxy: &CursorProxy,
    request: Request<Body>,
) -> Result<BufferedResponse> {
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map_or("/", |value| value.as_str());
    let url = upstream_url(&parts.headers, &proxy.upstream, path)?;
    let mut headers = parts.headers;
    headers.remove(harness::UPSTREAM_URL_HEADER);
    headers.remove(header::HOST);
    remove_hop_by_hop_headers(&mut headers);
    headers.insert(
        "connect-accept-encoding",
        axum::http::HeaderValue::from_static("identity"),
    );
    headers.insert(
        header::ACCEPT_ENCODING,
        axum::http::HeaderValue::from_static("identity"),
    );
    let body = to_bytes(body, MAX_BUFFERED_BYTES)
        .await
        .map_err(|error| crate::Error::Protocol(format!("cannot read request body: {error}")))?;
    let upstream = proxy
        .client
        .request(parts.method, url)
        .headers(headers)
        .body(body)
        .send()
        .await?;
    let status = upstream.status();
    let mut headers = upstream.headers().clone();
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
    let host = url.host_str().unwrap_or_default();
    if url.scheme() != "https" || !harness::is_cursor_host(host) {
        return Err(crate::Error::Protocol(
            "upstream URL must target a Cursor HTTPS host".into(),
        ));
    }
    Ok(url.into())
}

fn remove_hop_by_hop_headers(headers: &mut axum::http::HeaderMap) {
    let connection_headers = headers
        .get(header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
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

#[cfg(test)]
impl CursorProxy {
    #[expect(dead_code, reason = "test-only constructor")]
    pub fn with_upstream(upstream: String) -> Self {
        Self {
            client: upstream_client(),
            upstream,
        }
    }
}
