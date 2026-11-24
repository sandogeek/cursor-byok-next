//! The capabilities a plugin worker may reach: its own network calls, the
//! model request it was registered with, and the protocol adapters.
//!
//! [`PluginWorker`](super::worker::PluginWorker) owns the process and the line
//! protocol between it and Rust; this module owns what a `HostCall` frame
//! received over that protocol is allowed to do. Splitting them keeps the
//! transport free of policy: the worker cannot widen its own permissions, and
//! every check a plugin bypasses has to be changed here.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, OnceLock},
    time::Duration,
};

use serde_json::Value;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use super::{Error, Result, manifest::PluginPermissions, origin, wire, worker::InvocationState};
use crate::calls::CallRecorder;

/// Per-request deadline for a plugin's own `fetch`/`stream` calls. The worker's
/// own invocation deadline is longer, so a hanging upstream reports as a network
/// failure rather than a wedged worker.
const NETWORK_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// A buffered `fetch` reply must fit in memory; the runtime's job count does not
/// bound a hostile plugin's response size.
const MAX_NETWORK_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// One streamed reply may run for as long as its upstream takes, so only its
/// total size is capped.
const MAX_STREAM_BYTES: usize = 256 * 1024 * 1024;
/// How many queued lines one `network.stream.read` call returns. The plugin loop
/// asks again immediately, so a small batch keeps the reply bounded.
const STREAM_READ_BATCH: usize = 256;
/// Unread lines buffered between the reader task and the plugin. A stream that
/// outruns its consumer applies backpressure here instead of growing unbounded.
const STREAM_LINE_BUFFER: usize = 256;
/// Streams one invocation may keep open at once. Each holds a reader task and a
/// line queue, so an uncapped count is unbounded work per invocation.
const MAX_STREAMS_PER_INVOCATION: usize = 16;
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// The payload label used in `HostCall` argument errors.
const HOST_CALL: &str = "plugin host call";

/// The receiving half of one plugin stream, kept for `network.stream.read`.
/// The mutex makes reads sequential, so two `read` calls cannot interleave the
/// lines they return.
pub(super) type StreamLines = Arc<Mutex<mpsc::Receiver<Result<String>>>>;

/// A prepared plugin network call: everything `fetch`/`stream_open` needs to
/// run it and attribute the response.
pub(super) struct NetworkRequest {
    pub(super) request: reqwest::RequestBuilder,
    pub(super) origin: url::Url,
    pub(super) method: reqwest::Method,
    pub(super) recorder: Option<CallRecorder>,
}

/// Wraps a failed plugin network request, keeping reqwest's own error (and
/// therefore its whole source chain) instead of flattening it into a string.
fn provider_error(error: reqwest::Error) -> Error {
    Error::context("plugin network request failed".to_owned(), error)
}

/// What one plugin may reach from inside its worker. Cheap to clone: the worker
/// reader task holds one for the process's lifetime.
#[derive(Clone)]
pub(super) struct HostContext {
    plugin_id: String,
    network_hosts: Arc<HashSet<String>>,
    /// Whether a resource's own origin counts as allowed; the manifest decides.
    resource_origin: bool,
    /// Open streams keyed by stream id, shared across invocations. Entries are
    /// tagged with the owning invocation's cancellation token so the reader task
    /// can drop them when that invocation ends.
    streams: Arc<Mutex<HashMap<String, OpenStream>>>,
    /// Built on the first network call so a plugin with none never pays for it.
    http: OnceLock<reqwest::Client>,
    #[cfg(test)]
    sdk_projections: Arc<std::sync::atomic::AtomicUsize>,
}

/// A stream open under one invocation. Dropping it (via removal or invocation
/// cancellation) frees the reader task through the shared cancellation token.
struct OpenStream {
    lines: StreamLines,
    cancellation: CancellationToken,
}

impl HostContext {
    /// The context for one plugin, taking exactly the manifest's `permissions`.
    pub(super) fn new(plugin_id: &str, permissions: &PluginPermissions) -> Self {
        Self {
            plugin_id: plugin_id.to_owned(),
            network_hosts: Arc::new(
                permissions
                    .network
                    .iter()
                    .map(|host| host.to_ascii_lowercase())
                    .collect(),
            ),
            resource_origin: permissions.resource_origin,
            streams: Arc::new(Mutex::new(HashMap::new())),
            http: OnceLock::new(),
            #[cfg(test)]
            sdk_projections: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// The plugin id this context speaks for, used in worker log lines.
    pub(super) fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// Dispatches one `HostCall` frame. An unknown method is a protocol error:
    /// the worker SDK and this side ship together, so a mismatch is a bug rather
    /// than a plugin's mistake.
    pub(super) async fn call(
        &self,
        invocation: &InvocationState,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        match method {
            "network.fetch" => self.fetch(invocation, params).await,
            "network.stream.open" => self.stream_open(invocation, params).await,
            "network.stream.read" => self.stream_read(params).await,
            "network.stream.close" => {
                self.close_stream(&params).await?;
                Ok(Value::Null)
            }
            "protocol.stream" => super::adapters::stream(self, invocation, params).await,
            "provider.readRequest" => self.read_request(invocation),
            _ => Err(Error::Protocol(format!(
                "unsupported plugin host method: {method}"
            ))),
        }
    }

    #[cfg_attr(
        not(test),
        expect(
            clippy::unused_self,
            reason = "`self` is only read under cfg(test) to count SDK projections"
        )
    )]
    fn read_request(&self, invocation: &InvocationState) -> Result<Value> {
        #[cfg(test)]
        self.sdk_projections
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        wire::llm_request(invocation.model()?)
    }

    #[cfg(test)]
    pub(crate) fn sdk_projection_count(&self) -> usize {
        self.sdk_projections
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Validates the target and builds the request a `fetch`/`stream_open` (or
    /// the protocol adapters) will send, claiming the call recorder's request
    /// half when this invocation still has it.
    pub(super) async fn prepare_request(
        &self,
        invocation: &InvocationState,
        raw_url: &str,
        method: reqwest::Method,
        headers: &HashMap<String, String>,
        body: Option<&str>,
    ) -> Result<(reqwest::RequestBuilder, Option<CallRecorder>)> {
        let url = url::Url::parse(raw_url)
            .map_err(|error| Error::context("invalid plugin network URL".to_owned(), error))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Error::Config(
                "plugin network URL must not contain credentials".into(),
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| Error::Config("plugin network URL has no host".into()))?
            .to_ascii_lowercase();
        let origin = origin::request_origin(&url)?;
        let allowed_origin = invocation.allowed_origins.contains(&origin);
        let allowed_static_host = url.scheme() == "https" && self.network_hosts.contains(&host);
        if !(allowed_static_host || (self.resource_origin && allowed_origin)) {
            if url.scheme() != "https" && !self.resource_origin {
                return Err(Error::Config(
                    "plugin network URL must be HTTPS without credentials".into(),
                ));
            }
            return Err(Error::Config(format!(
                "plugin '{}' cannot access host '{host}'",
                self.plugin_id
            )));
        }
        let client = self.http_client()?;
        let mut request = client.request(method, url);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        if let Some(body) = body {
            request = request.body(body.to_owned());
        }
        let recorder = invocation.claim_recorder();
        if let Some(recorder) = &recorder {
            recorder
                .request(recorded_request(headers), &recorded_body(body))
                .await;
        }
        Ok((request, recorder))
    }

    fn http_client(&self) -> Result<&reqwest::Client> {
        if let Some(client) = self.http.get() {
            return Ok(client);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .pool_idle_timeout(HTTP_POOL_IDLE_TIMEOUT)
            .build()
            .map_err(|error| Error::context("cannot build plugin HTTP client".to_owned(), error))?;
        Ok(self.http.get_or_init(|| client))
    }

    pub(super) async fn request(
        &self,
        invocation: &InvocationState,
        params: &Value,
    ) -> Result<NetworkRequest> {
        let raw_url = wire::required_str(params, "url", HOST_CALL)?;
        let url = url::Url::parse(raw_url)
            .map_err(|error| Error::context("invalid plugin network URL".to_owned(), error))?;
        let method = params
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .parse::<reqwest::Method>()
            .map_err(|error| Error::context("invalid plugin HTTP method".to_owned(), error))?;
        let mut headers = HashMap::new();
        if let Some(map) = params.get("headers").and_then(Value::as_object) {
            for (name, value) in map {
                let value = value.as_str().ok_or_else(|| {
                    Error::Config(format!("plugin HTTP header '{name}' must be a string"))
                })?;
                headers.insert(name.clone(), value.to_owned());
            }
        }
        let body = params.get("body").and_then(Value::as_str);
        let (request, recorder) = self
            .prepare_request(invocation, raw_url, method.clone(), &headers, body)
            .await?;
        Ok(NetworkRequest {
            request,
            origin: url,
            method,
            recorder,
        })
    }

    async fn fetch(&self, invocation: &InvocationState, params: Value) -> Result<Value> {
        let NetworkRequest {
            request,
            origin: url,
            method,
            recorder,
        } = self.request(invocation, &params).await?;
        let origin = origin::request_origin(&url)?;
        // Magpie's model discovery is the one call whose status the app reports
        // as `catalogError`; every other status is the plugin's own business.
        let catalog_status = invocation.magpie_catalog_status.as_ref().filter(|_| {
            invocation.allowed_origins.contains(&origin)
                && method == reqwest::Method::GET
                && url.path() == "/v1/models"
        });
        let request = request.timeout(NETWORK_REQUEST_TIMEOUT);
        let mut response = tokio::select! {
            () = invocation.cancellation.cancelled() => return Err(Error::Cancelled),
            response = request.send() => response.map_err(provider_error)?,
        };
        let status = response.status().as_u16();
        if let Some(recorder) = &recorder {
            recorder.response_headers(status).await;
        }
        if let Some(catalog_status) = catalog_status {
            catalog_status.store(status, std::sync::atomic::Ordering::Relaxed);
            if !response.status().is_success() {
                return Ok(serde_json::json!({"status": status, "headers": {}, "body": ""}));
            }
        }
        if response.content_length().is_some_and(|size| {
            usize::try_from(size).map_or(true, |size| size > MAX_NETWORK_RESPONSE_BYTES)
        }) {
            return Err(response_too_large());
        }
        let headers = header_map(&response);
        let mut body = Vec::new();
        loop {
            let chunk = tokio::select! {
                () = invocation.cancellation.cancelled() => return Err(Error::Cancelled),
                chunk = response.chunk() => chunk.map_err(provider_error)?,
            };
            let Some(chunk) = chunk else { break };
            if chunk.len() > MAX_NETWORK_RESPONSE_BYTES.saturating_sub(body.len()) {
                return Err(response_too_large());
            }
            body.extend_from_slice(&chunk);
        }
        if let Some(recorder) = &recorder {
            recorder.response_chunk(&body).await;
        }
        Ok(serde_json::json!({
            "status": status,
            "headers": headers,
            "body": String::from_utf8_lossy(&body),
        }))
    }

    async fn stream_open(&self, invocation: &InvocationState, params: Value) -> Result<Value> {
        let NetworkRequest {
            request,
            origin: _,
            method: _,
            recorder,
        } = self.request(invocation, &params).await?;
        // The stream belongs to this invocation: when the invocation is cancelled
        // the reader task observes the token and stops, and the entry is dropped
        // on the next read/close. A plugin cannot leak one past its call.
        let cancellation = invocation.cancellation.clone();
        let response = tokio::select! {
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            response = request.send() => response.map_err(provider_error)?,
        };
        let status = response.status().as_u16();
        if let Some(recorder) = &recorder {
            recorder.response_headers(status).await;
        }
        let headers = header_map(&response);
        let stream_id = uuid::Uuid::new_v4().to_string();
        {
            let mut streams = self.streams.lock().await;
            prune_cancelled_streams(&mut streams);
            if streams.len() >= MAX_STREAMS_PER_INVOCATION {
                return Err(Error::Provider(
                    "too many open plugin network streams".into(),
                ));
            }
            let (sender, receiver) = mpsc::channel::<Result<String>>(STREAM_LINE_BUFFER);
            tokio::spawn(stream_body(
                response,
                sender,
                cancellation.clone(),
                recorder,
            ));
            streams.insert(
                stream_id.clone(),
                OpenStream {
                    lines: Arc::new(Mutex::new(receiver)),
                    cancellation,
                },
            );
        }
        Ok(serde_json::json!({
            "streamId": stream_id,
            "status": status,
            "headers": headers,
        }))
    }

    async fn stream_read(&self, params: Value) -> Result<Value> {
        let stream_id = wire::required_str(&params, "streamId", HOST_CALL)?;
        // Clone out the handle and release the map guard before waiting: the lock
        // must not be held across `recv()`.
        let lines_handle = {
            let mut streams = self.streams.lock().await;
            prune_cancelled_streams(&mut streams);
            streams
                .get(stream_id)
                .map(|stream| Arc::clone(&stream.lines))
                .ok_or_else(|| Error::Protocol(format!("unknown plugin stream: {stream_id}")))?
        };
        let mut receiver = lines_handle.lock().await;
        let mut lines = Vec::with_capacity(STREAM_READ_BATCH);
        match receiver.recv().await {
            Some(Ok(line)) => lines.push(line),
            Some(Err(error)) => {
                drop(receiver);
                self.streams.lock().await.remove(stream_id);
                return Err(error);
            }
            None => {
                drop(receiver);
                self.streams.lock().await.remove(stream_id);
                return Ok(serde_json::json!({ "lines": [], "done": true }));
            }
        }
        while lines.len() < STREAM_READ_BATCH {
            match receiver.try_recv() {
                Ok(Ok(line)) => lines.push(line),
                Ok(Err(error)) => {
                    drop(receiver);
                    self.streams.lock().await.remove(stream_id);
                    return Err(error);
                }
                Err(_) => break,
            }
        }
        Ok(serde_json::json!({ "lines": lines, "done": false }))
    }

    /// Removes one stream, cancelling its reader task.
    async fn close_stream(&self, params: &Value) -> Result<()> {
        let stream_id = wire::required_str(params, "streamId", HOST_CALL)?;
        if let Some(stream) = self.streams.lock().await.remove(stream_id) {
            stream.cancellation.cancel();
        }
        Ok(())
    }
}

/// Drops streams whose owning invocation was cancelled, so a plugin that never
/// calls `network.stream.close` cannot keep a reader task alive past its call.
fn prune_cancelled_streams(streams: &mut HashMap<String, OpenStream>) {
    streams.retain(|_, stream| !stream.cancellation.is_cancelled());
}

/// Splits a streaming reply into lines for `network.stream.read`, forwarding each
/// line as soon as its terminator arrives.
async fn stream_body(
    response: reqwest::Response,
    sender: mpsc::Sender<Result<String>>,
    cancellation: CancellationToken,
    recorder: Option<CallRecorder>,
) {
    use futures_util::StreamExt;
    let mut body = response.bytes_stream();
    let mut buffered = Vec::<u8>::new();
    let mut total = 0_usize;
    loop {
        let chunk = tokio::select! {
            () = cancellation.cancelled() => {
                send_terminal(&sender, Err(Error::Cancelled), &cancellation).await;
                return;
            }
            chunk = body.next() => chunk,
        };
        let Some(chunk) = chunk else { break };
        let Ok(chunk) = chunk else {
            fail(&sender, "cannot read plugin network stream", &cancellation).await;
            return;
        };
        total = total.saturating_add(chunk.len());
        if total > MAX_STREAM_BYTES {
            fail(
                &sender,
                "plugin network stream is larger than allowed",
                &cancellation,
            )
            .await;
            return;
        }
        if let Some(recorder) = &recorder {
            recorder.response_chunk(&chunk).await;
        }
        buffered.extend_from_slice(&chunk);
        // Drain forwards as each line is consumed, so a line that keeps arriving
        // in pieces never re-scans the whole buffer.
        let mut consumed = 0;
        while let Some(relative) = buffered[consumed..].iter().position(|byte| *byte == b'\n') {
            let end = consumed + relative;
            let mut line = buffered[consumed..end].to_vec();
            consumed = end + 1;
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            // The receiver may be full because the plugin stopped reading; the
            // cancellation branch means that no longer blocks this task forever.
            if !send_line(
                &sender,
                String::from_utf8_lossy(&line).into_owned(),
                &cancellation,
            )
            .await
            {
                return;
            }
        }
        buffered.copy_within(consumed.., 0);
        buffered.truncate(buffered.len() - consumed);
    }
    if !buffered.is_empty() {
        let line = String::from_utf8_lossy(&buffered).into_owned();
        let _ = send_line(&sender, line, &cancellation).await;
    }
}

/// Sends one line unless the invocation was cancelled. Returns whether the send
/// completed (so the caller knows whether to keep reading).
async fn send_line(
    sender: &mpsc::Sender<Result<String>>,
    line: String,
    cancellation: &CancellationToken,
) -> bool {
    tokio::select! {
        () = cancellation.cancelled() => false,
        result = sender.send(Ok(line)) => result.is_ok(),
    }
}

async fn fail(
    sender: &mpsc::Sender<Result<String>>,
    message: &str,
    cancellation: &CancellationToken,
) {
    send_terminal(sender, Err(Error::Provider(message.into())), cancellation).await;
}

/// Delivers a terminal message unless the invocation was cancelled; a full queue
/// is not awaited past cancellation.
async fn send_terminal(
    sender: &mpsc::Sender<Result<String>>,
    message: Result<String>,
    cancellation: &CancellationToken,
) {
    tokio::select! {
        () = cancellation.cancelled() => {}
        _ = sender.send(message) => {}
    }
}

fn response_too_large() -> Error {
    Error::Provider("plugin network response is larger than allowed".into())
}

/// The headers a call recorder keeps: every header but the credentials, so a
/// recorded call shows its shape without storing a key.
fn recorded_request(headers: &HashMap<String, String>) -> Value {
    let mut recorded = serde_json::Map::new();
    for (name, value) in headers {
        if !crate::calls::is_sensitive_header(name) {
            recorded.insert(name.clone(), value.clone().into());
        }
    }
    Value::Object(recorded)
}

/// A request body as JSON when it is JSON, so the call log shows a tree rather
/// than an escaped string.
fn recorded_body(body: Option<&str>) -> Value {
    body.map_or(Value::Null, |body| {
        serde_json::from_str(body).unwrap_or_else(|_| body.into())
    })
}

/// The response headers as a plain map, dropping any whose value is not text.
fn header_map(response: &reqwest::Response) -> BTreeMap<String, String> {
    response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.to_string(), value.to_string()))
        })
        .collect()
}
