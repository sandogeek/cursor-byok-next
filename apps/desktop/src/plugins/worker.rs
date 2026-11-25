//! Runs one thread-confined `QuickJS` VM per active trusted plugin.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{Mutex, mpsc, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{
    Error, Result,
    builtin::MAGPIE_PLUGIN_ID,
    catalog::PluginEntry,
    host_calls::HostContext,
    protocol::{HostMessage, WorkerMessage},
    vm::{self, loader::Modules},
    wire,
};
use crate::{calls::CallRecorder, model::ModelInvocation, provider::ModelEvent};

const INVOCATION_TIMEOUT: Duration = Duration::from_secs(60);

/// One item a plugin invocation produces: a streamed model event, or the
/// terminal result that ends the invocation.
#[derive(Debug)]
pub enum WorkerStreamItem {
    Event(ModelEvent),
    Result(Result<serde_json::Value>),
}

/// Complete frames in the bounded in-process session queue.
type Writer = std::sync::mpsc::SyncSender<Vec<u8>>;

/// Events one invocation may have queued for its consumer. Bounded so a plugin
/// that emits faster than the model pipeline drains cannot grow memory without
/// limit; a full queue ends the invocation instead of dropping events silently.
const EVENT_QUEUE_ITEMS: usize = 4_096;
/// Live invocations one worker serves at once. Each costs a task plus its event
/// queue, so a burst is capped rather than spawned without bound.
const MAX_ACTIVE_INVOCATIONS: usize = 16;
/// `host_call`s one invocation may have in flight. A plugin that fans out calls
/// without awaiting them is rejected past this instead of spawning forever.
const MAX_HOST_CALLS_PER_INVOCATION: usize = 8;
/// A registered stream: the guard that unregisters it, the stream the caller
/// reads events from, the sender the worker impl writes them to, and the
/// liveness watch that tells the SDK bridge the invocation is still running.
struct Registration {
    guard: InvocationGuard,
    receiver: mpsc::Receiver<WorkerStreamItem>,
    items: mpsc::Sender<WorkerStreamItem>,
    active: watch::Receiver<()>,
}

/// Per-invocation concurrency, so a plugin cannot spawn without bound.
pub(super) struct InvocationBudget {
    host_calls: std::sync::atomic::AtomicUsize,
}

impl InvocationBudget {
    fn new() -> Self {
        Self {
            host_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Takes one `host_call` slot, or reports the invocation is saturated.
    pub(super) fn try_acquire_host_call(&self) -> bool {
        let mut current = self.host_calls.load(Ordering::Relaxed);
        loop {
            if current >= MAX_HOST_CALLS_PER_INVOCATION {
                return false;
            }
            match self.host_calls.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    pub(super) fn release_host_call(&self) {
        self.host_calls.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) struct Pending(parking_lot::Mutex<PendingInvocationMap>);

/// The invocation registry while the worker runs, or `None` once it stopped.
type PendingInvocationMap = Option<HashMap<String, PendingInvocation>>;

struct PendingInvocation {
    items: mpsc::Sender<WorkerStreamItem>,
    active: watch::Sender<()>,
    state: Arc<InvocationState>,
    budget: Arc<InvocationBudget>,
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
    pub(super) fn open() -> Self {
        Self(parking_lot::Mutex::new(Some(HashMap::new())))
    }

    /// Registers one in-flight invocation. `self` stays an `Arc` at the call
    /// site because the returned guard must keep the map alive.
    fn register(self: &Arc<Self>, state: InvocationState) -> Result<Registration> {
        let id = uuid::Uuid::new_v4().to_string();
        let mut pending = self.0.lock();
        let map = pending
            .as_mut()
            .ok_or_else(|| Error::Provider("plugin worker stopped".into()))?;
        if map.len() >= MAX_ACTIVE_INVOCATIONS {
            return Err(Error::Provider(format!(
                "plugin worker is already running {MAX_ACTIVE_INVOCATIONS} invocations"
            )));
        }
        let (items, receiver) = mpsc::channel(EVENT_QUEUE_ITEMS);
        let (active, active_rx) = watch::channel(());
        *state.events.lock() = Some(items.clone());
        map.insert(
            id.clone(),
            PendingInvocation {
                items: items.clone(),
                active,
                state: Arc::new(state),
                budget: Arc::new(InvocationBudget::new()),
            },
        );
        Ok(Registration {
            guard: InvocationGuard {
                id,
                pending: Arc::clone(self),
            },
            receiver,
            items,
            active: active_rx,
        })
    }

    fn sender(&self, id: &str) -> Option<mpsc::Sender<WorkerStreamItem>> {
        self.0
            .lock()
            .as_ref()?
            .get(id)
            .map(|invocation| invocation.items.clone())
    }

    fn remove(&self, id: &str) -> Option<PendingInvocation> {
        let invocation = self.0.lock().as_mut()?.remove(id)?;
        invocation.state.cancellation.cancel();
        Some(invocation)
    }

    /// The state and liveness watch one `host_call` runs under, plus the budget
    /// the spawned call must hold a slot in.
    pub(super) fn authorization(
        &self,
        id: &str,
    ) -> Option<(
        Arc<InvocationState>,
        watch::Receiver<()>,
        Arc<InvocationBudget>,
    )> {
        let pending = self.0.lock();
        let invocation = pending.as_ref()?.get(id)?;
        Some((
            Arc::clone(&invocation.state),
            invocation.active.subscribe(),
            Arc::clone(&invocation.budget),
        ))
    }

    fn is_closed(&self) -> bool {
        self.0.lock().is_none()
    }

    pub(super) fn close(&self, message: &str) {
        let pending = self.0.lock().take();
        for (
            _,
            PendingInvocation {
                items,
                active,
                state,
                ..
            },
        ) in pending.into_iter().flatten()
        {
            state.cancellation.cancel();
            drop(active);
            // A full queue cannot take the terminal error; the consumer ends when
            // the sender drops with this map, so nothing is silently "completed".
            let _ = items.try_send(WorkerStreamItem::Result(Err(Error::Provider(
                message.into(),
            ))));
        }
    }
}

#[derive(Clone)]
pub struct PluginWorker {
    inner: Arc<PluginWorkerInner>,
}

/// One plugin invocation: what to call, how to reach the plugin's own
/// endpoints, and what to record.
pub(super) struct WorkerRequest<'a> {
    pub method: &'a str,
    pub params: serde_json::Value,
    pub cancellation: CancellationToken,
    pub recorder: Option<CallRecorder>,
    /// Origins the invocation's resource was registered for; a plugin may
    /// reach these even though they are not in its manifest.
    pub allowed_origins: HashSet<String>,
    pub model: Option<Arc<ModelInvocation>>,
}

impl std::fmt::Debug for PluginWorker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginWorker")
            .field("plugin_id", &self.inner.plugin_id)
            .field("entry", &self.inner.entry)
            .finish_non_exhaustive()
    }
}

struct PluginWorkerInner {
    plugin_id: String,
    entry: String,
    loader: Modules,
    host: HostContext,
    process: Mutex<Option<WorkerProcess>>,
    #[cfg(test)]
    last_invoke_frame: parking_lot::Mutex<Vec<u8>>,
}

struct WorkerProcess {
    session: vm::Session,
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

pub(super) struct InvocationState {
    pub(super) allowed_origins: HashSet<String>,
    /// Set only for Magpie's `models.list`, and only its own HTTP status is
    /// recorded: the app reports that status as `catalogError`.
    pub(super) magpie_catalog_status: Option<AtomicU16>,
    pub(super) cancellation: CancellationToken,
    recorder: Option<CallRecorder>,
    recorder_claimed: AtomicBool,
    model: Option<Arc<ModelInvocation>>,
    events: parking_lot::Mutex<Option<mpsc::Sender<WorkerStreamItem>>>,
}

impl InvocationState {
    pub(super) fn claim_recorder(&self) -> Option<CallRecorder> {
        self.recorder.as_ref().and_then(|recorder| {
            self.recorder_claimed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .ok()
                .map(|_| recorder.clone())
        })
    }

    pub(super) fn emit(&self, event: ModelEvent) {
        let Some(events) = self.events.lock().as_ref().cloned() else {
            return;
        };
        if events.try_send(WorkerStreamItem::Event(event)).is_err() {
            self.cancellation.cancel();
            // The consumer fell behind its budget. End the invocation with an
            // explicit error instead of dropping an event and continuing as
            // if the stream were still complete.
            let _ = events.try_send(WorkerStreamItem::Result(Err(Error::Provider(
                "plugin event queue overflowed; the invocation was ended".into(),
            ))));
        }
    }

    pub(super) fn model(&self) -> Result<&ModelInvocation> {
        self.model.as_deref().ok_or_else(|| {
            Error::Protocol("protocol.stream requires a registered model request".into())
        })
    }

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

impl PluginWorker {
    pub fn new(plugin: &PluginEntry, loader: Modules) -> Self {
        let plugin_id = plugin.manifest.id.clone();
        Self {
            inner: Arc::new(PluginWorkerInner {
                host: HostContext::new(&plugin_id, &plugin.manifest.permissions),
                plugin_id,
                entry: plugin.entry.clone(),
                loader,
                process: Mutex::new(None),
                #[cfg(test)]
                last_invoke_frame: parking_lot::Mutex::new(Vec::new()),
            }),
        }
    }

    /// Runs a non-streaming host call (`form.submit`, `models.list`,
    /// `resource.present`) and waits for its terminal result.
    ///
    /// # Errors
    ///
    /// Fails when the worker cannot be started or the frame cannot be encoded,
    /// when the plugin answers with an error, or when the invocation exceeds
    /// [`INVOCATION_TIMEOUT`].
    pub async fn invoke(
        &self,
        method: &str,
        params: serde_json::Value,
        allowed_origins: HashSet<String>,
    ) -> Result<serde_json::Value> {
        let mut items = self
            .invoke_streaming(WorkerRequest {
                method,
                params,
                cancellation: CancellationToken::new(),
                recorder: None,
                allowed_origins,
                model: None,
            })
            .await?;
        if let Ok(result) = tokio::time::timeout(INVOCATION_TIMEOUT, async {
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
        .await
        {
            result
        } else {
            self.stop().await;
            Err(Error::Provider("plugin invocation timed out".into()))
        }
    }

    /// Sends one request and hands back the stream of events and the terminal
    /// result.
    ///
    /// The caller must drain the receiver: cancelling `request.cancellation`
    /// or dropping the receiver withdraws the request from the worker.
    ///
    /// # Errors
    ///
    /// Fails when the worker cannot be started, no write permit is available,
    /// or the request frame cannot be encoded.
    pub async fn invoke_streaming(
        &self,
        request: WorkerRequest<'_>,
    ) -> Result<mpsc::Receiver<WorkerStreamItem>> {
        let WorkerRequest {
            method,
            params,
            cancellation,
            recorder,
            allowed_origins,
            model,
        } = request;
        let (writer, pending, stop) = self.connection().await?;
        let request_cancellation = CancellationToken::new();
        let Registration {
            guard: request,
            receiver,
            items,
            mut active,
        } = pending.register(InvocationState {
            allowed_origins,
            magpie_catalog_status: (self.inner.plugin_id == MAGPIE_PLUGIN_ID
                && method == "models.list")
                .then(AtomicU16::default),
            cancellation: request_cancellation.clone(),
            recorder,
            recorder_claimed: AtomicBool::new(false),
            model,
            events: parking_lot::Mutex::new(None),
        })?;
        let request_bytes = {
            let bytes = encode_message(&HostMessage::Request {
                id: request.id.clone(),
                method: method.to_owned(),
                params: params.clone(),
            })?;
            #[cfg(test)]
            if method == "provider.invoke" {
                *self.inner.last_invoke_frame.lock() = bytes.clone();
            }
            bytes
        };
        let request_id = request.id.clone();
        // One complete frame enters the bounded session queue, or registration is withdrawn.
        writer
            .try_send(request_bytes)
            .map_err(|_| Error::Runtime("plugin request queue is unavailable".into()))?;
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(INVOCATION_TIMEOUT) => { request_cancellation.cancel(); pending.close("plugin invocation timed out"); stop.store(true, Ordering::Release); }
                // Local teardown first: request-cancel, deregistration and the
                // error to the consumer never depend on the child still reading.
                () = cancellation.cancelled() => {
                    request_cancellation.cancel();
                    deregister(&pending, &request_id);
                    deliver_cancel(&writer, &request_id);
                }
                () = request_cancellation.cancelled() => {
                    deregister(&pending, &request_id);
                    deliver_cancel(&writer, &request_id);
                }
                () = items.closed() => {
                    request_cancellation.cancel();
                    deregister(&pending, &request_id);
                }
                _ = active.changed() => {}
            }
            drop(request);
        });
        Ok(receiver)
    }

    pub async fn stop(&self) {
        let process = self.inner.process.lock().await.take();
        if let Some(mut process) = process {
            process.session.stop.store(true, Ordering::Release);
            if let Some(thread) = process.session.thread.take() {
                let _ = tokio::task::spawn_blocking(move || thread.join()).await;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn last_invoke_frame(&self) -> Vec<u8> {
        self.inner.last_invoke_frame.lock().clone()
    }

    #[cfg(test)]
    pub(crate) fn sdk_projection_count(&self) -> usize {
        self.inner.host.sdk_projection_count()
    }

    async fn connection(&self) -> Result<(Writer, Arc<Pending>, Arc<AtomicBool>)> {
        let mut process = self.inner.process.lock().await;
        if process
            .as_ref()
            .is_none_or(|current| current.pending.is_closed())
        {
            *process = Some(self.spawn()?);
        }
        let Some(process) = process.as_ref() else {
            return Err(Error::Provider("plugin worker was not started".into()));
        };
        Ok((
            process.writer.clone(),
            Arc::clone(&process.pending),
            Arc::clone(&process.session.stop),
        ))
    }

    fn spawn(&self) -> Result<WorkerProcess> {
        let pending = Arc::new(Pending::open());
        let mut session = vm::spawn(
            self.inner.loader,
            self.inner.entry.clone(),
            &pending,
            self.inner.host.clone(),
        )?;
        let writer = session.writer.clone();
        let mut output = session
            .output
            .take()
            .ok_or_else(|| Error::Runtime("plugin output unavailable".into()))?;
        let task = tokio::spawn({
            let pending = Arc::clone(&pending);
            let host = self.inner.host.clone();
            let stop = Arc::clone(&session.stop);
            async move {
                while let Some(frame) = output.recv().await {
                    let Ok(message) = serde_json::from_slice::<WorkerMessage>(&frame) else {
                        break;
                    };
                    match message {
                        WorkerMessage::Result { id, result, error } => {
                            deliver_result(&pending, &id, result, error.as_deref());
                        }
                        WorkerMessage::Event { id, event } => {
                            deliver_event(&pending, &host, &id, &event);
                        }
                        WorkerMessage::HostCall { .. } => {
                            tracing::warn!(target: "plugin", "rejected legacy host_call frame");
                            break;
                        }
                    }
                }
                pending.close("plugin VM exited");
                stop.store(true, Ordering::Release);
            }
        });
        Ok(WorkerProcess {
            session,
            task,
            writer,
            pending,
        })
    }
}

/// Delivers a terminal `Result` frame, waking the consumer with the invocation's
/// own failure message when the plugin reported an error.
fn deliver_result(pending: &Pending, id: &str, result: serde_json::Value, error: Option<&str>) {
    if let Some(PendingInvocation {
        items,
        active,
        state,
        budget: _,
    }) = pending.remove(id)
    {
        drop(active);
        let value = if error.is_some() {
            Err(state.failure())
        } else {
            Ok(result)
        };
        let _ = items.try_send(WorkerStreamItem::Result(value));
    }
}

/// Delivers one streamed event, ending the invocation explicitly if the event
/// cannot be converted or the queue is full.
fn deliver_event(pending: &Pending, host: &HostContext, id: &str, event: &serde_json::Value) {
    let Some(sender) = pending.sender(id) else {
        return;
    };
    let event = match wire::model_event(event) {
        Ok(event) => event,
        Err(error) => {
            pending.remove(id);
            let _ = sender.try_send(WorkerStreamItem::Result(Err(error)));
            return;
        }
    };
    if sender.try_send(WorkerStreamItem::Event(event)).is_ok() {
        return;
    }
    tracing::warn!(
        plugin = host.plugin_id(),
        "plugin event queue overflowed; ending the invocation"
    );
    pending.remove(id);
    let _ = sender.try_send(WorkerStreamItem::Result(Err(Error::Provider(
        "plugin event queue overflowed; the invocation was ended".into(),
    ))));
}

/// Removes one invocation from the registry and tells its consumer it was
/// cancelled. Runs locally, independent of the VM thread.
fn deregister(pending: &Pending, request_id: &str) {
    if let Some(PendingInvocation { items, active, .. }) = pending.remove(request_id) {
        drop(active);
        let _ = items.try_send(WorkerStreamItem::Result(Err(Error::Cancelled)));
    }
}

/// Best-effort delivery of a `Cancel` frame. It must never block the teardown
/// path: if the VM stopped reading requests, the frame is dropped, which is
/// harmless because the invocation is already deregistered locally.
fn deliver_cancel(writer: &Writer, request_id: &str) {
    let Ok(bytes) = encode_message(&HostMessage::Cancel {
        id: request_id.to_owned(),
    }) else {
        return;
    };
    match writer.try_send(bytes) {
        Ok(()) | Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
        Err(std::sync::mpsc::TrySendError::Full(_)) => {
            tracing::debug!(
                "plugin session queue is full; dropping the cancel frame for a deregistered invocation"
            );
        }
    }
}

fn encode_message(message: &HostMessage) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(message)?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
fn recorded_network_request(
    params: &serde_json::Value,
) -> Result<(serde_json::Value, serde_json::Value)> {
    let mut recorded_headers = serde_json::Map::new();
    if let Some(headers) = params.get("headers").and_then(serde_json::Value::as_object) {
        for (name, value) in headers {
            let value = value.as_str().ok_or_else(|| {
                Error::Config(format!("plugin HTTP header '{name}' must be a string"))
            })?;
            if !crate::calls::is_sensitive_header(name) {
                recorded_headers.insert(name.clone(), value.into());
            }
        }
    }
    let body = params
        .get("body")
        .and_then(serde_json::Value::as_str)
        .map_or(serde_json::Value::Null, |body| {
            serde_json::from_str(body).unwrap_or_else(|_| body.into())
        });
    Ok((serde_json::Value::Object(recorded_headers), body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn invocation(allowed_origins: HashSet<String>) -> InvocationState {
        InvocationState {
            allowed_origins,
            magpie_catalog_status: None,
            cancellation: CancellationToken::new(),
            recorder: None,
            recorder_claimed: AtomicBool::new(false),
            model: None,
            events: parking_lot::Mutex::new(None),
        }
    }

    fn host() -> HostContext {
        HostContext::new(
            "test",
            &crate::plugins::manifest::PluginPermissions {
                network: Vec::new(),
                resource_origin: true,
            },
        )
    }

    #[tokio::test]
    async fn resource_origin_allows_bound_http_and_rejects_others() {
        let invocation = invocation(HashSet::from(["http://192.168.1.2:3425".into()]));
        let host = host();
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
    async fn protocol_stream_rejects_unregistered_requests_and_foreign_hosts() {
        let host = host();
        let missing = host
            .call(
                &invocation(HashSet::from(["http://127.0.0.1:1".into()])),
                "protocol.stream",
                serde_json::json!({
                    "protocol": "openai-chat",
                    "url": "http://127.0.0.1:1/v1/chat/completions",
                    "model": "gpt"
                }),
            )
            .await
            .unwrap_err();
        assert!(missing.to_string().contains("registered model request"));

        let mut registered = invocation(HashSet::from(["http://127.0.0.1:1".into()]));
        registered.model = Some(Arc::new(crate::model::ModelInvocation {
            call_id: "call".into(),
            run_id: "run".into(),
            conversation_id: "conversation".into(),
            provider_call_index: 0,
            request: crate::model::ModelRequest {
                prompt: crate::model::PromptSpec {
                    instructions: String::new(),
                    tools: Vec::new(),
                },
                model: crate::model::ModelSpec::new("plugin:p/c/m"),
                history: Vec::new(),
            },
        }));
        let foreign = host
            .call(
                &registered,
                "protocol.stream",
                serde_json::json!({
                    "protocol": "openai-chat",
                    "url": "http://10.0.0.2/v1/chat/completions",
                    "model": "gpt"
                }),
            )
            .await
            .unwrap_err();
        assert!(foreign.to_string().contains("cannot access host"));
        let credentials = host
            .call(
                &registered,
                "protocol.stream",
                serde_json::json!({
                    "protocol": "openai-chat",
                    "url": "http://user:pass@127.0.0.1:1/v1/chat/completions",
                    "model": "gpt"
                }),
            )
            .await
            .unwrap_err();
        assert!(
            credentials
                .to_string()
                .contains("must not contain credentials")
        );

        let projected = host
            .call(&registered, "provider.readRequest", serde_json::Value::Null)
            .await
            .unwrap();
        assert_eq!(projected["cacheKey"], "conversation");
        assert_eq!(host.sdk_projection_count(), 1);
        let again = host
            .call(&registered, "provider.readRequest", serde_json::Value::Null)
            .await
            .unwrap();
        assert_eq!(again, projected);
        assert_eq!(host.sdk_projection_count(), 2);
    }

    #[tokio::test]
    async fn catalog_invocations_cannot_read_the_model_request() {
        let error = host()
            .call(
                &invocation(HashSet::new()),
                "provider.readRequest",
                serde_json::Value::Null,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("registered model request"));
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
        let host = host();
        let invocation = InvocationState {
            allowed_origins: HashSet::from([origin.clone()]),
            magpie_catalog_status: Some(AtomicU16::default()),
            cancellation: CancellationToken::new(),
            recorder: None,
            recorder_claimed: AtomicBool::new(false),
            model: None,
            events: parking_lot::Mutex::new(None),
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
        let Registration {
            guard: completed,
            mut receiver,
            ..
        } = pending.register(invocation(HashSet::new())).unwrap();
        let (_, mut active, _) = pending.authorization(&completed.id).unwrap();
        let PendingInvocation {
            items,
            active: signal,
            ..
        } = pending.remove(&completed.id).unwrap();
        drop(signal);
        items
            .try_send(WorkerStreamItem::Result(Ok(serde_json::Value::Null)))
            .unwrap();
        match receiver.recv().await {
            Some(WorkerStreamItem::Result(Ok(value))) => assert!(value.is_null()),
            other => panic!("expected result, got {other:?}"),
        }
        assert!(active.changed().await.is_err());
        assert!(pending.authorization(&completed.id).is_none());

        let Registration {
            guard: cancelled,
            mut receiver,
            ..
        } = pending.register(invocation(HashSet::new())).unwrap();
        let id = cancelled.id.clone();
        let (_, mut active, _) = pending.authorization(&id).unwrap();
        drop(cancelled);
        assert!(receiver.recv().await.is_none());
        assert!(active.changed().await.is_err());
        assert!(pending.authorization(&id).is_none());
    }

    #[tokio::test]
    async fn invocation_timeout_drops_its_pending_entry() {
        let pending = Arc::new(Pending::open());
        let mut id = None;
        let timed_out = tokio::time::timeout(Duration::from_millis(1), async {
            let Registration {
                guard: request,
                receiver: mut result,
                ..
            } = pending.register(invocation(HashSet::new())).unwrap();
            id = Some(request.id.clone());
            result.recv().await
        })
        .await;
        assert!(timed_out.is_err());
        assert!(pending.authorization(&id.unwrap()).is_none());
    }

    #[tokio::test]
    async fn closing_one_generation_revokes_only_its_invocations() {
        let old = Arc::new(Pending::open());
        let Registration {
            guard: old_request,
            receiver: mut failed,
            ..
        } = old.register(invocation(HashSet::new())).unwrap();
        let (_, mut active, _) = old.authorization(&old_request.id).unwrap();
        let new = Arc::new(Pending::open());
        let Registration {
            guard: new_request,
            receiver: mut unaffected,
            ..
        } = new.register(invocation(HashSet::new())).unwrap();
        old.close("plugin worker exited");
        match failed.recv().await {
            Some(WorkerStreamItem::Result(Err(error))) => {
                assert!(error.to_string().contains("worker exited"));
            }
            other => panic!("expected failed result, got {other:?}"),
        }
        assert!(active.changed().await.is_err());
        assert!(old.authorization(&old_request.id).is_none());
        assert!(old.register(invocation(HashSet::new())).is_err());
        assert!(new.authorization(&new_request.id).is_some());
        assert!(matches!(
            unaffected.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    fn network_params() -> serde_json::Value {
        serde_json::json!({
            "url": "https://example.com/v1/responses",
            "method": "POST",
            "headers": {
                "Authorization": "Bearer secret",
                "X-Api-Key": "secret-key",
                "Cookie": "session=secret",
                "content-type": "application/json",
                "x-client-request-id": "request-1"
            },
            "body": "{\"model\":\"test\",\"stream\":true}"
        })
    }

    async fn recorder(
        storage: crate::calls::PayloadStorage,
        call_id: &str,
    ) -> (tempfile::TempDir, crate::calls::CallStore, CallRecorder) {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::calls::CallStore::at(directory.path().join("calls.db"));
        store.set_detailed_logging(storage).await.unwrap();
        let recorder = CallRecorder::start(
            store.clone(),
            crate::calls::NewLlmCall {
                call_id: call_id.into(),
                run_id: "run".into(),
                conversation_id: "conversation".into(),
                provider_call_index: 0,
                model_hash: "plugin:test/provider/model".into(),
                provider_type: "plugin".into(),
                provider_url: "plugin://test/provider".into(),
                request_type: "plugin".into(),
                request_url: "plugin://test/provider".into(),
                model_id: "model".into(),
                display_name: "Model".into(),
                reasoning_effort: None,
                fast: false,
                message_count: 1,
                tool_count: 0,
                detailed: false,
            },
        )
        .await;
        (directory, store, recorder)
    }

    fn recorded_invocation(recorder: CallRecorder) -> InvocationState {
        InvocationState {
            allowed_origins: HashSet::new(),
            magpie_catalog_status: None,
            cancellation: CancellationToken::new(),
            recorder: Some(recorder),
            recorder_claimed: AtomicBool::new(false),
            model: None,
            events: parking_lot::Mutex::new(None),
        }
    }

    fn recording_host() -> HostContext {
        HostContext::new(
            "test",
            &crate::plugins::manifest::PluginPermissions {
                network: vec!["example.com".into()],
                resource_origin: false,
            },
        )
    }

    #[test]
    fn recorded_plugin_request_omits_sensitive_headers() {
        let (headers, body) = recorded_network_request(&network_params()).unwrap();
        assert_eq!(
            headers,
            serde_json::json!({
                "content-type": "application/json",
                "x-client-request-id": "request-1"
            })
        );
        assert_eq!(body, serde_json::json!({ "model": "test", "stream": true }));
    }

    #[tokio::test]
    async fn only_the_first_plugin_network_request_claims_the_recorder() {
        let (_directory, store, recorder) =
            recorder(crate::calls::PayloadStorage::Detailed, "first-request").await;
        let host = recording_host();
        let invocation = recorded_invocation(recorder.clone());
        let first = host.request(&invocation, &network_params()).await.unwrap();
        let second = host.request(&invocation, &network_params()).await.unwrap();
        assert!(first.recorder.is_some());
        assert!(second.recorder.is_none());
        recorder.response_headers(200).await;
        recorder
            .response_chunk(b"data: {\"type\":\"response.created\"}\n\n")
            .await;
        recorder.response_chunk(b"data: [DONE]\n\n").await;
        recorder
            .completed(crate::provider::FinishReason::Stop)
            .await;
        let request = store
            .llm_call_request("first-request")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            request.headers,
            serde_json::json!({
                "content-type": "application/json",
                "x-client-request-id": "request-1"
            })
        );
        let mut transcript = serde_json::to_string(&request.headers).unwrap();
        transcript.push_str(&serde_json::to_string(&request.body).unwrap());
        assert!(!transcript.contains("secret"));
        let chunks = store.llm_call_chunks("first-request").await.unwrap();
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.data.as_str())
                .collect::<String>(),
            "data: {\"type\":\"response.created\"}\n\ndata: [DONE]\n\n"
        );
    }

    #[tokio::test]
    async fn standard_plugin_network_recording_keeps_metrics_without_payloads() {
        let (_directory, store, recorder) =
            recorder(crate::calls::PayloadStorage::Metrics, "standard-plugin").await;
        let host = recording_host();
        let invocation = recorded_invocation(recorder.clone());
        let params = network_params();
        let (_, body) = recorded_network_request(&params).unwrap();
        let request_bytes =
            crate::saturating::i64_from_usize(serde_json::to_string(&body).unwrap().len());
        let claimed = host.request(&invocation, &params).await.unwrap();
        assert!(claimed.recorder.is_some());
        recorder.response_headers(204).await;
        recorder.response_chunk(b"data: [DONE]\n\n").await;
        recorder
            .completed(crate::provider::FinishReason::Stop)
            .await;
        assert!(
            store
                .llm_call_request("standard-plugin")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.llm_call_chunks("standard-plugin").await.unwrap(),
            Vec::<crate::calls::LlmCallResponseChunk>::new()
        );
        let summary = store.llm_call("standard-plugin").await.unwrap().unwrap();
        assert_eq!(summary.http_status, Some(204));
        assert_eq!(summary.request_bytes, Some(request_bytes));
        assert_eq!(summary.response_bytes, 14);
        assert!(!summary.detailed);
    }

    async fn echo_worker(_root: &std::path::Path, source: &str) -> (Modules, PluginWorker) {
        // The fixtures are JS modules; author-facing TS compilation has separate build tests.
        let source = source
            .replace(
                "import type { ProviderSupport } from \"cursor-byok-next:provider\";",
                "",
            )
            .replace(": ProviderSupport", "");
        let mut table = super::super::builtin::MODULES.to_vec();
        table.push(("echo/main.js", Box::leak(source.into_boxed_str())));
        let loader = Modules(Box::leak(table.into_boxed_slice()));
        let definition = vm::collect(loader, "echo/main.js".into()).await.unwrap();
        let providers = super::super::catalog::validate_definition(
            "dev.cursorbyoknext.plugins.echo",
            &definition,
        )
        .unwrap();
        let plugin = super::super::catalog::PluginEntry {
            entry: "echo/main.js".into(),
            manifest: serde_json::from_value(serde_json::json!({"apiVersion":1,"id":"dev.cursorbyoknext.plugins.echo","name":"Echo","version":"0.1.0","icon":"assets/magpie.svg","entry":"echo/main.js","permissions":{"network":[],"resourceOrigin":false}})).unwrap(),
            definition, providers, icon: String::new(),
        };
        (loader, PluginWorker::new(&plugin, loader))
    }

    fn echo_invocation(text: &str) -> Arc<crate::model::ModelInvocation> {
        Arc::new(crate::model::ModelInvocation {
            call_id: "call".into(),
            run_id: "run".into(),
            conversation_id: "conversation".into(),
            provider_call_index: 0,
            request: crate::model::ModelRequest {
                prompt: crate::model::PromptSpec {
                    instructions: "be brief".into(),
                    tools: Vec::new(),
                },
                model: crate::model::ModelSpec::new("plugin:echo/echo/echo"),
                history: vec![crate::model::ProjectedMessage {
                    message_id: "m1".into(),
                    role: crate::model::Role::User,
                    content: crate::model::ProjectedContent::Parts(vec![
                        crate::model::ContentPart::Text { text: text.into() },
                    ]),
                }],
            },
        })
    }

    const ECHO_PLUGIN: &str = r#"
import { defineProviderPlugin } from "cursor-byok-next:plugin";
import type { ProviderSupport } from "cursor-byok-next:provider";

const provider: ProviderSupport = {
  id: "echo",
  displayName: "Echo",
  providerType: "echo",
  async invoke(input, output) {
    const first = await input.readRequest();
    const second = await input.readRequest();
    output.emit({ type: "text-start" });
    output.emit({
      type: "text-delta",
      text: JSON.stringify({
        same: JSON.stringify(first) === JSON.stringify(second),
        instructions: first.instructions,
        text: first.messages[0].content[0].text,
        cacheKey: first.cacheKey,
      }),
    });
    output.emit({ type: "text-end" });
    output.emit({ type: "done", reason: "stop" });
    return { status: "completed" };
  },
};

export default defineProviderPlugin({ providers: [provider] });
"#;

    async fn invoke_echo(
        worker: &PluginWorker,
        invocation: Arc<crate::model::ModelInvocation>,
        cancellation: CancellationToken,
    ) -> Vec<crate::provider::ModelEvent> {
        let mut items = worker
            .invoke_streaming(WorkerRequest {
                method: "provider.invoke",
                params: serde_json::json!({
                    "providerId": "echo",
                    "model": { "id": "echo", "displayName": "Echo" },
                    "resource": null
                }),
                cancellation,
                recorder: None,
                allowed_origins: HashSet::new(),
                model: Some(invocation),
            })
            .await
            .unwrap();
        let mut events = Vec::new();
        while let Some(item) = items.recv().await {
            match item {
                WorkerStreamItem::Event(event) => events.push(event),
                WorkerStreamItem::Result(result) => {
                    result.unwrap();
                    break;
                }
            }
        }
        events
    }

    #[tokio::test]
    async fn custom_plugin_reads_the_sdk_request_once_per_invoke() {
        let temp = tempfile::tempdir().unwrap();
        let (_loader, worker) = echo_worker(temp.path(), ECHO_PLUGIN).await;
        let events = invoke_echo(&worker, echo_invocation("hello"), CancellationToken::new()).await;
        let text = events.iter().find_map(|event| match event {
            crate::provider::ModelEvent::TextDelta(text) => Some(text.as_str()),
            _ => None,
        });
        let payload: serde_json::Value = serde_json::from_str(text.unwrap()).unwrap();
        assert_eq!(payload["same"], true);
        assert_eq!(payload["instructions"], "be brief");
        assert_eq!(payload["text"], "hello");
        assert_eq!(payload["cacheKey"], "conversation");
        assert_eq!(worker.sdk_projection_count(), 1);
        worker.stop().await;
    }

    #[tokio::test]
    async fn concurrent_invokes_cannot_read_each_others_requests() {
        let temp = tempfile::tempdir().unwrap();
        let (_loader, worker) = echo_worker(temp.path(), ECHO_PLUGIN).await;
        let first = invoke_echo(&worker, echo_invocation("alpha"), CancellationToken::new());
        let second = invoke_echo(&worker, echo_invocation("beta"), CancellationToken::new());
        let (first, second) = tokio::join!(first, second);
        let texts = |events: &[crate::provider::ModelEvent]| {
            events
                .iter()
                .find_map(|event| match event {
                    crate::provider::ModelEvent::TextDelta(text) => Some(text.as_str()),
                    _ => None,
                })
                .unwrap()
                .to_owned()
        };
        let first: serde_json::Value = serde_json::from_str(&texts(&first)).unwrap();
        let second: serde_json::Value = serde_json::from_str(&texts(&second)).unwrap();
        assert_eq!(first["text"], "alpha");
        assert_eq!(second["text"], "beta");
        worker.stop().await;
    }

    #[tokio::test]
    async fn cancelled_invokes_cannot_read_the_model_request() {
        let source = r#"
import { defineProviderPlugin } from "cursor-byok-next:plugin";
import type { ProviderSupport } from "cursor-byok-next:provider";

const provider: ProviderSupport = {
  id: "echo",
  displayName: "Echo",
  providerType: "echo",
  async invoke(input) {
    await new Promise((resolve) => setTimeout(resolve, 200));
    try {
      await input.readRequest();
      return { status: "completed" };
    } catch (error) {
      return { status: "request-error", message: error instanceof Error ? error.message : String(error) };
    }
  },
};

export default defineProviderPlugin({ providers: [provider] });
"#;
        let temp = tempfile::tempdir().unwrap();
        let (_loader, worker) = echo_worker(temp.path(), source).await;
        let cancellation = CancellationToken::new();
        let mut items = worker
            .invoke_streaming(WorkerRequest {
                method: "provider.invoke",
                params: serde_json::json!({
                    "providerId": "echo",
                    "model": { "id": "echo", "displayName": "Echo" },
                    "resource": null
                }),
                cancellation: cancellation.clone(),
                recorder: None,
                allowed_origins: HashSet::new(),
                model: Some(echo_invocation("hello")),
            })
            .await
            .unwrap();
        cancellation.cancel();
        let result = loop {
            match items.recv().await {
                Some(WorkerStreamItem::Result(result)) => break result,
                Some(WorkerStreamItem::Event(_)) => {}
                None => panic!("worker stopped before returning a result"),
            }
        };
        match result {
            Err(error) => assert!(error.to_string().contains("cancelled")),
            Ok(value) => assert_eq!(value["status"], "request-error"),
        }
        worker.stop().await;
    }

    #[test]
    fn invoke_handles_are_taken_before_the_request_is_sent() {
        let pending = Arc::new(Pending::open());
        let Registration {
            guard,
            mut receiver,
            items,
            active,
        } = pending.register(invocation(HashSet::new())).unwrap();
        pending.remove(&guard.id);
        let _ = items.try_send(WorkerStreamItem::Result(Ok(
            serde_json::json!({"ok": true}),
        )));
        drop(active);
        let Some(WorkerStreamItem::Result(Ok(value))) = receiver.try_recv().ok() else {
            panic!("pre-send handles must still receive the worker result");
        };
        assert_eq!(value["ok"], true);
        drop(guard);
    }

    #[tokio::test]
    async fn register_rejects_more_than_the_active_invocation_budget() {
        let pending = Arc::new(Pending::open());
        let mut guards = Vec::new();
        for _ in 0..MAX_ACTIVE_INVOCATIONS {
            guards.push(pending.register(invocation(HashSet::new())).unwrap());
        }
        let overflow = pending.register(invocation(HashSet::new()));
        assert!(overflow.is_err(), "a burst must be capped");
        drop(guards);
    }

    #[test]
    fn event_overflow_ends_the_invocation_instead_of_dropping_events() {
        let pending = Arc::new(Pending::open());
        let Registration {
            guard,
            mut receiver,
            items,
            active,
        } = pending.register(invocation(HashSet::new())).unwrap();
        let cancellation = pending
            .authorization(&guard.id)
            .unwrap()
            .0
            .cancellation
            .clone();
        let event = serde_json::json!({"type":"text-start"});
        for _ in 0..EVENT_QUEUE_ITEMS {
            deliver_event(&pending, &host(), &guard.id, &event);
        }
        assert_eq!(receiver.len(), EVENT_QUEUE_ITEMS);
        deliver_event(&pending, &host(), &guard.id, &event);
        assert!(pending.authorization(&guard.id).is_none());
        assert!(cancellation.is_cancelled());
        assert!(active.has_changed().is_err());
        drop(items);
        let mut count = 0;
        while let Ok(item) = receiver.try_recv() {
            assert!(matches!(item, WorkerStreamItem::Event(_)));
            count += 1;
        }
        assert_eq!(count, EVENT_QUEUE_ITEMS);
        assert!(
            receiver.is_closed(),
            "overflow must close the stream, never fabricate a successful result"
        );
    }
    #[tokio::test]
    async fn native_calls_are_concurrent_but_the_ninth_is_rejected() {
        let source = r#"
import { defineProviderPlugin } from "cursor-byok-next:plugin";
export default defineProviderPlugin({providers:[{id:'echo',displayName:'Echo',providerType:'echo',async invoke(input, output, context) {
 const results = await Promise.allSettled(Array.from({length:9},()=>context.network.fetch('https://not-allowed.invalid/')));
 output.emit({type:'text-delta',text:JSON.stringify(results.map(result=>result.status==='rejected'?String(result.reason):'resolved'))});
 return {status:'completed'};
}}]});
"#;
        let temp = tempfile::tempdir().unwrap();
        let (_, worker) = echo_worker(temp.path(), source).await;
        let events = invoke_echo(&worker, echo_invocation("hello"), CancellationToken::new()).await;
        let text = events
            .iter()
            .find_map(|event| {
                if let ModelEvent::TextDelta(text) = event {
                    Some(text)
                } else {
                    None
                }
            })
            .unwrap();
        let results: Vec<String> = serde_json::from_str(text).unwrap();
        assert_eq!(results.len(), 9);
        assert!(
            results[8].contains("too many concurrent host calls"),
            "{results:?}"
        );
        assert!(
            results[..8]
                .iter()
                .all(|error| error.contains("ERR_ACCESS_DENIED")),
            "{results:?}"
        );
        worker.stop().await;
    }

    #[tokio::test]
    async fn a_stopped_vm_is_rebuilt_on_the_next_connection() {
        let temp = tempfile::tempdir().unwrap();
        let (_, worker) = echo_worker(temp.path(), ECHO_PLUGIN).await;
        let before = invoke_echo(&worker, echo_invocation("first"), CancellationToken::new()).await;
        worker.stop().await;
        let after = invoke_echo(&worker, echo_invocation("second"), CancellationToken::new()).await;
        assert_ne!(before.len(), 0);
        assert_ne!(after.len(), 0);
        worker.stop().await;
    }

    #[tokio::test]
    async fn session_frames_are_golden_and_capabilities_are_not_frames() {
        let modules = Modules(Box::leak(
            vec![(
                "sdk/worker.js",
                r"
globalThis.__recv = async line => {
 const message=JSON.parse(line);
 await __host(message.id, 'network.fetch', JSON.stringify({url:'http://example.invalid'})).catch(()=>null);
 if(message.method==='provider.invoke') __send(JSON.stringify({type:'event',id:message.id,event:{type:'text-start'}}));
 __send(JSON.stringify({type:'result',id:message.id,result:null}));
};
",
            )]
            .into_boxed_slice(),
        ));
        let pending = Arc::new(Pending::open());
        let mut session = vm::spawn(modules, "unused.js".into(), &pending, host()).unwrap();
        let mut output = session.output.take().unwrap();
        for method in ["provider.invoke", "models.list"] {
            let registration = pending.register(invocation(HashSet::new())).unwrap();
            let id = &registration.guard.id;
            let request =
                format!(r#"{{"type":"request","id":"{id}","method":"{method}","params":{{}}}}"#);
            session.writer.try_send(request.into_bytes()).unwrap();
            if method == "provider.invoke" {
                assert_eq!(
                    output.recv().await.unwrap(),
                    format!(r#"{{"type":"event","id":"{id}","event":{{"type":"text-start"}}}}"#)
                        .as_bytes()
                );
            }
            assert_eq!(
                output.recv().await.unwrap(),
                format!(r#"{{"type":"result","id":"{id}","result":null}}"#).as_bytes()
            );
            assert!(
                output.try_recv().is_err(),
                "models.list has one result; capability completions must never enter the session queue"
            );
        }
        session.stop.store(true, Ordering::Release);
        let thread = session.thread.take().unwrap();
        tokio::task::spawn_blocking(move || thread.join().unwrap())
            .await
            .unwrap();
    }
    async fn network_probe(
        worker: &PluginWorker,
        origin: &str,
        text: &str,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<WorkerStreamItem> {
        worker.invoke_streaming(WorkerRequest {
            method: "provider.invoke",
            params: serde_json::json!({"providerId":"echo","model":{"id":"echo","displayName":"Echo"},"resource":null}),
            cancellation, recorder: None,
            allowed_origins: HashSet::from([origin.to_owned()]),
            model: Some(echo_invocation(text)),
        }).await.unwrap()
    }

    async fn tasks_drained(host: &HostContext) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while host.active_task_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn native_promises_resolve_reject_and_preserve_only_safe_http_status() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let app = axum::Router::new()
            .route(
                "/fetch",
                axum::routing::get(|| async { (axum::http::StatusCode::CREATED, "ok") }),
            )
            .route(
                "/error",
                axum::routing::post(|| async {
                    (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        "secret-upstream-body",
                    )
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let source = r"
import { defineProviderPlugin } from 'cursor-byok-next:plugin';
export default defineProviderPlugin({providers:[{
 id:'echo',displayName:'Echo',providerType:'echo',async invoke(input, output, context) {
  const reply=await context.network.fetch('ORIGIN/fetch');
  let errorStatus, errorMessage, denied;
  try { await context.protocol.stream({protocol:'openai-chat',url:'ORIGIN/error',model:'echo'}); }
  catch(error) { errorStatus=error.status; errorMessage=error.message; }
  try { await context.network.fetch('http://example.invalid/forbidden'); }
  catch(error) { denied=String(error); }
  output.emit({type:'text-delta',text:JSON.stringify({status:reply.status,body:reply.body,errorStatus,errorMessage,denied})});
  return {status:'completed'};
 }
}]});
".replace("ORIGIN", &origin);
        let temp = tempfile::tempdir().unwrap();
        let (_, mut worker) = echo_worker(temp.path(), &source).await;
        Arc::get_mut(&mut worker.inner).unwrap().host = host();
        let mut items = network_probe(&worker, &origin, "start", CancellationToken::new()).await;
        let mut payload = None;
        while let Some(item) = items.recv().await {
            match item {
                WorkerStreamItem::Event(crate::provider::ModelEvent::TextDelta(text)) => {
                    payload = Some(serde_json::from_str::<serde_json::Value>(&text).unwrap());
                }
                WorkerStreamItem::Result(result) => {
                    result.unwrap();
                    break;
                }
                WorkerStreamItem::Event(_) => {}
            }
        }
        let payload = payload.unwrap();
        assert_eq!(payload["status"], 201);
        assert_eq!(payload["body"], "ok");
        assert_eq!(payload["errorStatus"], 503);
        assert!(
            payload["denied"]
                .as_str()
                .unwrap()
                .contains("ERR_ACCESS_DENIED")
        );
        assert!(!payload.to_string().contains("secret-upstream-body"));
        tasks_drained(&worker.inner.host).await;
        worker.stop().await;
        server.abort();
    }

    #[tokio::test]
    async fn cancellation_aborts_signal_rejects_native_promises_and_drains_tasks() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (accepted, ready) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            let _ = accepted.send(());
            std::future::pending::<()>().await;
        });
        let source = r"
import { defineProviderPlugin } from 'cursor-byok-next:plugin';
let aborted=false, rejected=false;
export default defineProviderPlugin({providers:[{
 id:'echo',displayName:'Echo',providerType:'echo',async invoke(input, output, context) {
  const request=await input.readRequest();
  if(request.messages[0].content[0].text==='start') {
   context.signal.addEventListener('abort',()=>{aborted=context.signal.aborted;});
   try { await context.network.fetch('ORIGIN/hold'); } catch { rejected=true; }
  } else { output.emit({type:'text-delta',text:JSON.stringify({aborted,rejected})}); }
  return {status:'completed'};
 }
}]});
"
        .replace("ORIGIN", &origin);
        let temp = tempfile::tempdir().unwrap();
        let (_, mut worker) = echo_worker(temp.path(), &source).await;
        Arc::get_mut(&mut worker.inner).unwrap().host = host();
        let cancellation = CancellationToken::new();
        let mut items = network_probe(&worker, &origin, "start", cancellation.clone()).await;
        tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(worker.inner.host.active_task_count(), 1);
        cancellation.cancel();
        if let Some(item) = items.recv().await {
            assert!(matches!(item, WorkerStreamItem::Result(Err(_))));
        }
        tasks_drained(&worker.inner.host).await;
        let events = invoke_echo(
            &worker,
            echo_invocation("inspect"),
            CancellationToken::new(),
        )
        .await;
        let text = events
            .iter()
            .find_map(|event| match event {
                crate::provider::ModelEvent::TextDelta(text) => Some(text),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(text).unwrap(),
            serde_json::json!({"aborted":true,"rejected":true})
        );
        worker.stop().await;
        server.abort();
    }

    #[tokio::test]
    async fn network_stream_limits_and_ownership_are_per_invocation_and_eof_cleans_tasks() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let app = axum::Router::new().route(
            "/events",
            axum::routing::get(|| async { "data: first\n\ndata: second\n\n" }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let host = host();
        let first = invocation(HashSet::from([origin.clone()]));
        let second = invocation(HashSet::from([origin.clone()]));
        let mut ids = Vec::new();
        for _ in 0..MAX_HOST_CALLS_PER_INVOCATION * 2 {
            let opened = host
                .call(
                    &first,
                    "network.stream.open",
                    serde_json::json!({"url":format!("{origin}/events")}),
                )
                .await
                .unwrap();
            ids.push(opened["streamId"].as_str().unwrap().to_owned());
        }
        let error = host
            .call(
                &first,
                "network.stream.open",
                serde_json::json!({"url":format!("{origin}/events")}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("too many open"));
        let own = host
            .call(
                &second,
                "network.stream.open",
                serde_json::json!({"url":format!("{origin}/events")}),
            )
            .await
            .unwrap();
        assert!(
            host.call(
                &second,
                "network.stream.read",
                serde_json::json!({"streamId":ids[0]})
            )
            .await
            .is_err()
        );
        assert!(
            host.call(
                &second,
                "network.stream.close",
                serde_json::json!({"streamId":ids[0]})
            )
            .await
            .is_err()
        );
        let mut lines = Vec::new();
        loop {
            let chunk = host
                .call(
                    &first,
                    "network.stream.read",
                    serde_json::json!({"streamId":ids[0]}),
                )
                .await
                .unwrap();
            lines.extend(chunk["lines"].as_array().unwrap().iter().cloned());
            if chunk["done"] == true {
                break;
            }
        }
        assert!(lines.contains(&serde_json::json!("data: first")));
        assert!(lines.contains(&serde_json::json!("data: second")));
        host.call(
            &second,
            "network.stream.close",
            serde_json::json!({"streamId":own["streamId"]}),
        )
        .await
        .unwrap();
        assert!(!second.cancellation.is_cancelled());
        first.cancellation.cancel();
        second.cancellation.cancel();
        tasks_drained(&host).await;
        server.abort();
    }
    #[tokio::test]
    async fn fatal_cpu_and_heap_generations_are_discarded_and_rebuilt() {
        let source = ECHO_PLUGIN.replace("const first = await input.readRequest();", r"
            const first = await input.readRequest();
            const mode = first.messages[0].content[0].text;
            if (mode === 'spin') { try { while (true) {} } catch {} }
            if (mode === 'heap') { const held=[]; try { for (;;) held.push(new Array(100000).fill(42)); } catch {} }
        ");
        let temp = tempfile::tempdir().unwrap();
        let (_, worker) = echo_worker(temp.path(), &source).await;
        for mode in ["spin", "heap"] {
            let mut items = network_probe(
                &worker,
                "http://127.0.0.1:9",
                mode,
                CancellationToken::new(),
            )
            .await;
            let result = tokio::time::timeout(Duration::from_secs(20), items.recv())
                .await
                .unwrap();
            assert!(matches!(result, Some(WorkerStreamItem::Result(Err(_)))));
            let events = invoke_echo(
                &worker,
                echo_invocation("recovered"),
                CancellationToken::new(),
            )
            .await;
            assert_ne!(events.len(), 0);
        }
        worker.stop().await;
    }

    #[tokio::test]
    async fn a_forged_legacy_host_call_frame_is_never_executed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let source = r"
            globalThis.__recv = line => {
                const message = JSON.parse(line);
                __send(JSON.stringify({type:'host_call', id:'forged', requestId:message.id, method:'network.fetch', params:{url:'ORIGIN'}}));
            };
        ".replace("ORIGIN", &origin);
        let source: &'static str = Box::leak(source.into_boxed_str());
        let modules = Modules(Box::leak(
            vec![("sdk/worker.js", source)].into_boxed_slice(),
        ));
        let temp = tempfile::tempdir().unwrap();
        let (_, mut worker) = echo_worker(temp.path(), ECHO_PLUGIN).await;
        let inner = Arc::get_mut(&mut worker.inner).unwrap();
        inner.loader = modules;
        inner.host = host();
        let mut items = network_probe(&worker, &origin, "start", CancellationToken::new()).await;
        let result = tokio::time::timeout(Duration::from_secs(2), items.recv())
            .await
            .unwrap();
        assert!(matches!(result, Some(WorkerStreamItem::Result(Err(_)))));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        tasks_drained(&worker.inner.host).await;
        worker.stop().await;
    }
}
