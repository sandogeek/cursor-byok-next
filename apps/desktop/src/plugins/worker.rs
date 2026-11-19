//! Runs one long-lived, sandboxed Bun process per active plugin.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
    time::Duration,
};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::ChildStdin,
    sync::{Mutex, mpsc, oneshot, watch},
    task::JoinHandle,
};

use super::{
    Error, Result,
    catalog::PluginEntry,
    definition::{PluginDefinitionLoader, file_url},
    origin,
    protocol::{HostMessage, WorkerMessage},
};

const INVOCATION_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_NETWORK_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;

type InvocationResult = Result<serde_json::Value>;
type Writer = mpsc::Sender<Vec<u8>>;

// One registry per process generation. None means the generation cannot accept new requests.
struct Pending(parking_lot::Mutex<Option<HashMap<String, PendingInvocation>>>);

struct PendingInvocation {
    result: oneshot::Sender<InvocationResult>,
    active: watch::Sender<()>,
    state: Arc<InvocationState>,
}

struct InvocationGuard {
    id: String,
    pending: Arc<Pending>,
}

impl Drop for InvocationGuard {
    fn drop(&mut self) {
        self.pending.remove(&self.id);
    }
}

impl Pending {
    fn open() -> Self {
        Self(parking_lot::Mutex::new(Some(HashMap::new())))
    }

    fn register(
        self: &Arc<Self>,
        state: InvocationState,
    ) -> Result<(InvocationGuard, oneshot::Receiver<InvocationResult>)> {
        let id = uuid::Uuid::new_v4().to_string();
        let (result, receiver) = oneshot::channel();
        let (active, _) = watch::channel(());
        self.0
            .lock()
            .as_mut()
            .ok_or_else(|| Error::Provider("plugin worker stopped".into()))?
            .insert(
                id.clone(),
                PendingInvocation {
                    result,
                    active,
                    state: Arc::new(state),
                },
            );
        Ok((
            InvocationGuard {
                id,
                pending: self.clone(),
            },
            receiver,
        ))
    }

    fn remove(&self, id: &str) -> Option<PendingInvocation> {
        self.0.lock().as_mut()?.remove(id)
    }

    fn authorization(&self, id: &str) -> Option<(Arc<InvocationState>, watch::Receiver<()>)> {
        let pending = self.0.lock();
        let invocation = pending.as_ref()?.get(id)?;
        Some((invocation.state.clone(), invocation.active.subscribe()))
    }

    fn is_closed(&self) -> bool {
        self.0.lock().is_none()
    }

    fn close(&self, message: &str) {
        let pending = self.0.lock().take();
        for (_, PendingInvocation { result, active, .. }) in pending.into_iter().flatten() {
            drop(active);
            let _ = result.send(Err(Error::Provider(message.into())));
        }
    }
}

#[derive(Clone)]
pub struct PluginWorker {
    inner: Arc<PluginWorkerInner>,
}

struct PluginWorkerInner {
    plugin_id: String,
    executable: PathBuf,
    entry: PathBuf,
    loader: PluginDefinitionLoader,
    host: HostContext,
    process: Mutex<Option<WorkerProcess>>,
}

struct WorkerProcess {
    task: JoinHandle<()>,
    writer: Writer,
    pending: Arc<Pending>,
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        self.pending.close("plugin worker stopped");
        self.task.abort();
    }
}

struct InvocationState {
    allowed_origins: HashSet<String>,
    magpie_catalog_status: Option<AtomicU16>,
}

impl InvocationState {
    fn failure(&self) -> Error {
        let status = self
            .magpie_catalog_status
            .as_ref()
            .map(|status| status.load(Ordering::Relaxed));
        let message = match status {
            Some(status @ (100..=199 | 300..=599)) => {
                format!("Magpie model discovery failed (HTTP {status})")
            }
            _ => "plugin invocation failed".into(),
        };
        Error::Provider(message)
    }
}

#[derive(Clone)]
struct HostContext {
    plugin_id: String,
    network_hosts: Arc<HashSet<String>>,
    resource_origin: bool,
}

impl PluginWorker {
    pub fn new(plugin: &PluginEntry, executable: PathBuf, loader: PluginDefinitionLoader) -> Self {
        let plugin_id = plugin.manifest.id.clone();
        Self {
            inner: Arc::new(PluginWorkerInner {
                host: HostContext {
                    plugin_id: plugin_id.clone(),
                    network_hosts: Arc::new(
                        plugin
                            .manifest
                            .permissions
                            .network
                            .iter()
                            .map(|host| host.to_ascii_lowercase())
                            .collect(),
                    ),
                    resource_origin: plugin.manifest.permissions.resource_origin,
                },
                plugin_id,
                executable,
                entry: plugin.entry.clone(),
                loader,
                process: Mutex::new(None),
            }),
        }
    }

    pub async fn invoke(
        &self,
        method: &str,
        params: serde_json::Value,
        allowed_origins: HashSet<String>,
    ) -> Result<serde_json::Value> {
        match tokio::time::timeout(INVOCATION_TIMEOUT, async {
            let (writer, pending) = self.connection().await?;
            let permit = writer
                .reserve()
                .await
                .map_err(|_| Error::Provider("plugin worker stopped".into()))?;
            let (request, result) = pending.register(InvocationState {
                allowed_origins,
                magpie_catalog_status: (self.inner.plugin_id
                    == "dev.cursorbyoknext.plugins.magpie"
                    && method == "models.list")
                    .then(AtomicU16::default),
            })?;
            permit.send(encode_message(&HostMessage::Request {
                id: &request.id,
                method,
                params: &params,
            })?);
            result
                .await
                .map_err(|_| Error::Provider("plugin worker stopped".into()))?
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(Error::Provider(format!(
                "plugin '{}' invocation timed out",
                self.inner.plugin_id
            ))),
        }
    }

    #[allow(dead_code)]
    pub async fn stop(&self) {
        self.inner.process.lock().await.take();
    }

    async fn connection(&self) -> Result<(Writer, Arc<Pending>)> {
        let mut process = self.inner.process.lock().await;
        if process
            .as_ref()
            .is_none_or(|current| current.pending.is_closed())
        {
            *process = Some(self.spawn()?);
        }
        let process = process.as_ref().expect("plugin worker was started");
        Ok((process.writer.clone(), process.pending.clone()))
    }

    fn spawn(&self) -> Result<WorkerProcess> {
        let entry_url = file_url(&self.inner.entry)?;
        let mut command = self.inner.loader.bun_command(&self.inner.executable);
        command
            .arg(self.inner.loader.worker_path())
            .arg(entry_url.as_str())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            Error::Config(format!(
                "cannot start plugin worker {}: {error}",
                self.inner.executable.display()
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Config("cannot open plugin worker stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Config("cannot open plugin worker stdout".into()))?;
        let pending = Arc::new(Pending::open());
        let (writer, messages) = mpsc::channel(32);
        let task = tokio::spawn({
            let plugin_id = self.inner.plugin_id.clone();
            let writer = writer.clone();
            let pending = pending.clone();
            let host = self.inner.host.clone();
            async move {
                tokio::select! {
                    _ = read_stdout(stdout, &writer, &pending, &host) => {},
                    _ = write_stdin(stdin, messages) => {},
                    _ = child.wait() => {},
                }
                pending.close(&format!("plugin '{plugin_id}' worker exited"));
            }
        });
        Ok(WorkerProcess {
            task,
            writer,
            pending,
        })
    }
}

async fn read_stdout(
    stdout: tokio::process::ChildStdout,
    writer: &Writer,
    pending: &Pending,
    host: &HostContext,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let message = match serde_json::from_str::<WorkerMessage>(&line) {
            Ok(message) => message,
            Err(_) => {
                tracing::warn!(plugin = %host.plugin_id, "plugin worker wrote an invalid message");
                continue;
            }
        };
        match message {
            WorkerMessage::Result { id, result, error } => {
                if let Some(PendingInvocation {
                    result: sender,
                    active,
                    state,
                }) = pending.remove(&id)
                {
                    drop(active);
                    let value = if error.is_some() {
                        Err(state.failure())
                    } else {
                        Ok(result)
                    };
                    let _ = sender.send(value);
                }
            }
            WorkerMessage::Event { id, event } => {
                // This caller does not consume streaming model events yet.
                drop((id, event));
            }
            WorkerMessage::HostCall {
                id,
                request_id,
                method,
                params,
            } => {
                let Some((invocation, mut active)) = pending.authorization(&request_id) else {
                    continue;
                };
                let host = host.clone();
                let writer = writer.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        biased;
                        _ = active.changed() => {},
                        _ = async {
                            let response = match host.call(&invocation, &method, params).await {
                                Ok(result) => encode_message(&HostMessage::HostResult { id: &id, result: &result }),
                                Err(_) => encode_message(&HostMessage::HostError { id: &id, error: "plugin host call failed" }),
                            };
                            if let Ok(bytes) = response {
                                let _ = writer.send(bytes).await;
                            }
                        } => {},
                    }
                });
            }
        }
    }
}

fn encode_message(message: &HostMessage<'_>) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(message)?;
    bytes.push(b'\n');
    Ok(bytes)
}

// A single writer finishes each frame even if its requesting future is dropped.
async fn write_stdin(
    mut stdin: ChildStdin,
    mut messages: mpsc::Receiver<Vec<u8>>,
) -> std::io::Result<()> {
    while let Some(bytes) = messages.recv().await {
        stdin.write_all(&bytes).await?;
        stdin.flush().await?;
    }
    Ok(())
}

impl HostContext {
    async fn call(
        &self,
        invocation: &InvocationState,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        match method {
            "network.fetch" => self.fetch(invocation, params).await,
            "network.stream.open" | "network.stream.read" | "network.stream.close" => {
                Err(Error::Protocol(
                    "plugin streaming is not available until model invocation is implemented"
                        .into(),
                ))
            }
            _ => Err(Error::Protocol(format!(
                "unsupported plugin host method: {method}"
            ))),
        }
    }

    async fn fetch(
        &self,
        invocation: &InvocationState,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let raw_url = required_string(&params, "url")?;
        let url = url::Url::parse(raw_url)
            .map_err(|error| Error::Config(format!("invalid plugin network URL: {error}")))?;
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
        if !allowed_static_host && !(self.resource_origin && allowed_origin) {
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
        let method = params
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("GET")
            .parse::<reqwest::Method>()
            .map_err(|error| Error::Config(format!("invalid plugin HTTP method: {error}")))?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|error| Error::Config(format!("cannot build plugin HTTP client: {error}")))?;
        // Only host-observed status from the bound Magpie catalog GET is exposed on failure.
        let catalog_status = invocation.magpie_catalog_status.as_ref().filter(|_| {
            allowed_origin && method == reqwest::Method::GET && url.path() == "/v1/models"
        });
        let mut request = client.request(method, url);
        if let Some(headers) = params.get("headers").and_then(serde_json::Value::as_object) {
            for (name, value) in headers {
                let value = value.as_str().ok_or_else(|| {
                    Error::Config(format!("plugin HTTP header '{name}' must be a string"))
                })?;
                request = request.header(name, value);
            }
        }
        if let Some(body) = params.get("body").and_then(serde_json::Value::as_str) {
            request = request.body(body.to_owned());
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| Error::Provider("plugin network request failed".into()))?;
        let status = response.status().as_u16();
        if let Some(catalog_status) = catalog_status {
            catalog_status.store(status, Ordering::Relaxed);
            // Magpie only needs the status for a failed catalog response.
            if !response.status().is_success() {
                return Ok(serde_json::json!({"status": status, "headers": {}, "body": ""}));
            }
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_NETWORK_RESPONSE_BYTES)
        {
            return Err(Error::Provider(
                "plugin network response is larger than allowed".into(),
            ));
        }
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.to_string(), value.to_string()))
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Error::Provider("cannot read plugin network response".into()))?
        {
            if chunk.len() as u64 > MAX_NETWORK_RESPONSE_BYTES - body.len() as u64 {
                return Err(Error::Provider(
                    "plugin network response is larger than allowed".into(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(serde_json::json!({
            "status": status,
            "headers": headers,
            "body": String::from_utf8_lossy(&body),
        }))
    }
}

fn required_string<'a>(params: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    params
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::Protocol(format!("plugin host call requires string '{key}'")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocation(allowed_origins: HashSet<String>) -> InvocationState {
        InvocationState {
            allowed_origins,
            magpie_catalog_status: None,
        }
    }

    #[tokio::test]
    async fn resource_origin_allows_bound_http_and_rejects_others() {
        let invocation = invocation(HashSet::from(["http://192.168.1.2:3425".into()]));
        let host = HostContext {
            plugin_id: "test".into(),
            network_hosts: Arc::new(HashSet::new()),
            resource_origin: true,
        };
        let error = host
            .call(
                &invocation,
                "network.fetch",
                serde_json::json!({ "url": "http://10.0.0.2:3425/v1/models" }),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cannot access host"));
    }

    #[tokio::test]
    async fn magpie_catalog_reports_only_the_host_observed_status() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert_ne!(socket.read(&mut request).await.unwrap(), 0);
            socket
                .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 11\r\nConnection: close\r\n\r\nmock detail")
                .await
                .unwrap();
        });
        let host = HostContext {
            plugin_id: "test".into(),
            network_hosts: Arc::new(HashSet::new()),
            resource_origin: true,
        };
        let invocation = InvocationState {
            allowed_origins: HashSet::from([origin.clone()]),
            magpie_catalog_status: Some(AtomicU16::default()),
        };
        assert!(!invocation.failure().to_string().contains("HTTP"));
        let result = host
            .call(
                &invocation,
                "network.fetch",
                serde_json::json!({ "url": format!("{origin}/v1/models") }),
            )
            .await
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!({"status": 401, "headers": {}, "body": ""})
        );
        assert!(invocation.failure().to_string().contains("HTTP 401"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pending_authorization_ends_on_completion_and_drop() {
        let pending = Arc::new(Pending::open());
        let (completed, receiver) = pending.register(invocation(HashSet::new())).unwrap();
        let (_, mut active) = pending.authorization(&completed.id).unwrap();
        let PendingInvocation {
            result,
            active: signal,
            ..
        } = pending.remove(&completed.id).unwrap();
        drop(signal);
        result.send(Ok(serde_json::Value::Null)).unwrap();
        assert!(receiver.await.unwrap().unwrap().is_null());
        assert!(active.changed().await.is_err());
        assert!(pending.authorization(&completed.id).is_none());

        let (cancelled, receiver) = pending.register(invocation(HashSet::new())).unwrap();
        let id = cancelled.id.clone();
        let (_, mut active) = pending.authorization(&id).unwrap();
        drop(cancelled);
        assert!(receiver.await.is_err());
        assert!(active.changed().await.is_err());
        assert!(pending.authorization(&id).is_none());
    }

    #[tokio::test]
    async fn invocation_timeout_drops_its_pending_entry() {
        let pending = Arc::new(Pending::open());
        let mut id = None;
        let timed_out = tokio::time::timeout(Duration::from_millis(1), async {
            let (request, result) = pending.register(invocation(HashSet::new())).unwrap();
            id = Some(request.id.clone());
            result.await
        })
        .await;
        assert!(timed_out.is_err());
        assert!(pending.authorization(&id.unwrap()).is_none());
    }

    #[tokio::test]
    async fn closing_one_generation_revokes_only_its_invocations() {
        let old = Arc::new(Pending::open());
        let (old_request, failed) = old.register(invocation(HashSet::new())).unwrap();
        let (_, mut active) = old.authorization(&old_request.id).unwrap();
        let new = Arc::new(Pending::open());
        let (new_request, mut unaffected) = new.register(invocation(HashSet::new())).unwrap();
        old.close("plugin worker exited");
        assert!(
            failed
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("worker exited")
        );
        assert!(active.changed().await.is_err());
        assert!(old.authorization(&old_request.id).is_none());
        assert!(old.register(invocation(HashSet::new())).is_err());
        assert!(new.authorization(&new_request.id).is_some());
        assert!(matches!(
            unaffected.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
    }
}
