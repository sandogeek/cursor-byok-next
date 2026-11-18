//! Runs one long-lived, sandboxed Bun process per active plugin.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin},
    sync::{mpsc, Mutex},
};

use super::{
    catalog::PluginEntry,
    definition::{file_url, PluginDefinitionLoader},
    origin,
    protocol::{HostMessage, WorkerMessage},
    Error, Result,
};

const INVOCATION_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_NETWORK_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug)]
pub enum WorkerStreamItem {
    #[allow(dead_code)]
    Event(serde_json::Value),
    Result(Result<serde_json::Value>),
}

type Pending = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<WorkerStreamItem>>>>;

#[derive(Clone)]
pub struct PluginWorker {
    inner: Arc<PluginWorkerInner>,
}

struct PluginWorkerInner {
    plugin_id: String,
    executable: PathBuf,
    directory: PathBuf,
    entry: PathBuf,
    loader: PluginDefinitionLoader,
    host: HostContext,
    process: Mutex<Option<WorkerProcess>>,
    pending: Pending,
}

struct WorkerProcess {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
}

struct InvocationState {
    allowed_origins: HashSet<String>,
}

#[derive(Clone)]
struct HostContext {
    plugin_id: String,
    network_hosts: Arc<HashSet<String>>,
    resource_origin: bool,
    invocations: Arc<Mutex<HashMap<String, Arc<InvocationState>>>>,
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
                    invocations: Arc::new(Mutex::new(HashMap::new())),
                },
                plugin_id,
                executable,
                directory: plugin.directory.clone(),
                entry: plugin.entry.clone(),
                loader,
                process: Mutex::new(None),
                pending: Arc::new(Mutex::new(HashMap::new())),
            }),
        }
    }

    pub async fn invoke(
        &self,
        method: &str,
        params: serde_json::Value,
        allowed_origins: HashSet<String>,
    ) -> Result<serde_json::Value> {
        let mut items = self
            .invoke_streaming(method, params, allowed_origins)
            .await?;
        let result = tokio::time::timeout(INVOCATION_TIMEOUT, async {
            while let Some(item) = items.recv().await {
                if let WorkerStreamItem::Result(result) = item {
                    return result;
                }
            }
            Err(Error::Provider(format!(
                "plugin '{}' worker stopped",
                self.inner.plugin_id
            )))
        })
        .await;
        match result {
            Ok(result) => result,
            Err(_) => Err(Error::Provider(format!(
                "plugin '{}' invocation timed out",
                self.inner.plugin_id
            ))),
        }
    }

    async fn invoke_streaming(
        &self,
        method: &str,
        params: serde_json::Value,
        allowed_origins: HashSet<String>,
    ) -> Result<mpsc::UnboundedReceiver<WorkerStreamItem>> {
        let id = uuid::Uuid::new_v4().to_string();
        self.inner.host.invocations.lock().await.insert(
            id.clone(),
            Arc::new(InvocationState { allowed_origins }),
        );
        let (sender, receiver) = mpsc::unbounded_channel();
        self.inner
            .pending
            .lock()
            .await
            .insert(id.clone(), sender.clone());
        let send_result = async {
            let stdin = self.stdin().await?;
            write_message(
                &stdin,
                &HostMessage::Request {
                    id: &id,
                    method,
                    params: &params,
                },
            )
            .await
        }
        .await;
        if let Err(error) = send_result {
            self.cleanup(&id).await;
            return Err(error);
        }
        Ok(receiver)
    }

    #[allow(dead_code)]
    pub async fn stop(&self) {
        if let Some(mut process) = self.inner.process.lock().await.take() {
            let _ = process.child.kill().await;
        }
        fail_pending(&self.inner.pending, "plugin worker stopped").await;
    }

    async fn cleanup(&self, id: &str) {
        self.inner.pending.lock().await.remove(id);
        self.inner.host.invocations.lock().await.remove(id);
    }

    async fn stdin(&self) -> Result<Arc<Mutex<ChildStdin>>> {
        let mut process = self.inner.process.lock().await;
        let dead = match process.as_mut() {
            Some(current) => current
                .child
                .try_wait()
                .map_err(|error| {
                    Error::Config(format!("cannot check plugin worker status: {error}"))
                })?
                .is_some(),
            None => true,
        };
        if dead {
            *process = Some(self.spawn().await?);
        }
        Ok(process
            .as_ref()
            .expect("plugin worker was started")
            .stdin
            .clone())
    }

    async fn spawn(&self) -> Result<WorkerProcess> {
        let entry_url = file_url(&self.inner.entry)?;
        let mut command = tokio::process::Command::new(&self.inner.executable);
        super::detach_console(&mut command);
        command
            .arg("run")
            .arg("--no-install")
            .arg(format!("--config={}", self.inner.loader.bunfig().display()))
            .arg(self.inner.loader.worker_path())
            .arg(entry_url.as_str())
            .current_dir(&self.inner.directory)
            .env("BUN_INSTALL_CACHE_DIR", self.inner.loader.sdk_dir().join("cache"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            Error::Config(format!(
                "cannot start plugin worker {}: {error}",
                self.inner.executable.display()
            ))
        })?;
        let stdin =
            Arc::new(Mutex::new(child.stdin.take().ok_or_else(|| {
                Error::Config("cannot open plugin worker stdin".into())
            })?));
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Config("cannot open plugin worker stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Config("cannot open plugin worker stderr".into()))?;
        spawn_stdout_reader(
            self.inner.plugin_id.clone(),
            stdout,
            stdin.clone(),
            self.inner.pending.clone(),
            self.inner.host.clone(),
        );
        spawn_stderr_reader(self.inner.plugin_id.clone(), stderr);
        Ok(WorkerProcess { child, stdin })
    }
}

fn spawn_stdout_reader(
    plugin_id: String,
    stdout: tokio::process::ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    host: HostContext,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let message = match serde_json::from_str::<WorkerMessage>(&line) {
                Ok(message) => message,
                Err(error) => {
                    tracing::warn!(plugin = %plugin_id, %error, "plugin worker wrote an invalid message");
                    continue;
                }
            };
            match message {
                WorkerMessage::Result { id, result, error } => {
                    if let Some(sender) = pending.lock().await.remove(&id) {
                        let value = match error {
                            Some(error) => {
                                Err(Error::Provider(format!("plugin '{plugin_id}': {error}")))
                            }
                            None => Ok(result),
                        };
                        let _ = sender.send(WorkerStreamItem::Result(value));
                    }
                }
                WorkerMessage::Event { id, event } => {
                    if let Some(sender) = pending.lock().await.get(&id) {
                        let _ = sender.send(WorkerStreamItem::Event(event));
                    }
                }
                WorkerMessage::HostCall {
                    id,
                    request_id,
                    method,
                    params,
                } => {
                    let host = host.clone();
                    let stdin = stdin.clone();
                    tokio::spawn(async move {
                        let result = host.call(&request_id, &method, params).await;
                        match result {
                            Ok(result) => {
                                let _ = write_message(
                                    &stdin,
                                    &HostMessage::HostResult {
                                        id: &id,
                                        result: &result,
                                    },
                                )
                                .await;
                            }
                            Err(error) => {
                                let text = error.to_string();
                                let _ = write_message(
                                    &stdin,
                                    &HostMessage::HostError {
                                        id: &id,
                                        error: &text,
                                    },
                                )
                                .await;
                            }
                        }
                    });
                }
            }
        }
        fail_pending(&pending, &format!("plugin '{plugin_id}' worker exited")).await;
    });
}

fn spawn_stderr_reader(plugin_id: String, stderr: tokio::process::ChildStderr) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::warn!(plugin = %plugin_id, message = %line, "plugin worker stderr");
        }
    });
}

async fn write_message(stdin: &Arc<Mutex<ChildStdin>>, message: &HostMessage<'_>) -> Result<()> {
    let mut bytes = serde_json::to_vec(message)?;
    bytes.push(b'\n');
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(&bytes)
        .await
        .map_err(|error| Error::Config(format!("cannot write to plugin worker: {error}")))?;
    stdin
        .flush()
        .await
        .map_err(|error| Error::Config(format!("cannot flush plugin worker stdin: {error}")))?;
    Ok(())
}

async fn fail_pending(pending: &Pending, message: &str) {
    for (_, sender) in std::mem::take(&mut *pending.lock().await) {
        let _ = sender.send(WorkerStreamItem::Result(Err(Error::Provider(
            message.into(),
        ))));
    }
}

impl HostContext {
    async fn call(
        &self,
        request_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        match method {
            "network.fetch" => self.fetch(request_id, params).await,
            "network.stream.open" | "network.stream.read" | "network.stream.close" => {
                Err(Error::Protocol(
                    "plugin streaming is not available until model invocation is implemented".into(),
                ))
            }
            _ => Err(Error::Protocol(format!(
                "unsupported plugin host method: {method}"
            ))),
        }
    }

    async fn fetch(
        &self,
        request_id: &str,
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
        let invocation = self.invocations.lock().await.get(request_id).cloned();
        let allowed_origin = invocation
            .as_ref()
            .is_some_and(|state| state.allowed_origins.contains(&origin));
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
        let response = request
            .send()
            .await
            .map_err(|error| Error::Provider(error.to_string()))?;
        let status = response.status().as_u16();
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
        let body = response
            .bytes()
            .await
            .map_err(|error| Error::Provider(error.to_string()))?;
        if body.len() as u64 > MAX_NETWORK_RESPONSE_BYTES {
            return Err(Error::Provider(
                "plugin network response is larger than allowed".into(),
            ));
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

    #[tokio::test]
    async fn resource_origin_allows_bound_http_and_rejects_others() {
        let invocations = Arc::new(Mutex::new(HashMap::new()));
        invocations.lock().await.insert(
            "invocation".into(),
            Arc::new(InvocationState {
                allowed_origins: HashSet::from(["http://192.168.1.2:3425".into()]),
            }),
        );
        let host = HostContext {
            plugin_id: "test".into(),
            network_hosts: Arc::new(HashSet::new()),
            resource_origin: true,
            invocations,
        };
        let error = host
            .call(
                "invocation",
                "network.fetch",
                serde_json::json!({ "url": "http://10.0.0.2:3425/v1/models" }),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cannot access host"));
    }
}
