//! Runs one long-lived, sandboxed Bun process per active plugin.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    time::Duration,
};

use tokio::{
    io::AsyncWriteExt,
    process::ChildStdin,
    sync::{Mutex, mpsc, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{
    Error, Result,
    builtin::MAGPIE_PLUGIN_ID,
    catalog::PluginEntry,
    definition::{PluginDefinitionLoader, file_url},
    host_calls::HostContext,
    protocol::{HostMessage, WorkerMessage},
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

/// Frames queued for the single stdin writer, as bytes ready to send.
type Writer = mpsc::Sender<Vec<u8>>;

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
/// Longest single NDJSON line read from a worker's stdout or stderr. Output that
/// never ends with a newline must be cut off rather than buffered indefinitely.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

#[cfg(test)]
static LAST_INVOKE_FRAME: parking_lot::Mutex<Vec<u8>> = parking_lot::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn last_invoke_frame() -> Vec<u8> {
    LAST_INVOKE_FRAME.lock().clone()
}

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
struct InvocationBudget {
    host_calls: std::sync::atomic::AtomicUsize,
}

impl InvocationBudget {
    fn new() -> Self {
        Self {
            host_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Takes one `host_call` slot, or reports the invocation is saturated.
    fn try_acquire_host_call(&self) -> bool {
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

    fn release_host_call(&self) {
        self.host_calls.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Pending(parking_lot::Mutex<PendingInvocationMap>);

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
    fn open() -> Self {
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
        self.0.lock().as_mut()?.remove(id)
    }

    /// The state and liveness watch one `host_call` runs under, plus the budget
    /// the spawned call must hold a slot in.
    fn authorization(
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

    fn close(&self, message: &str) {
        let pending = self.0.lock().take();
        for (_, PendingInvocation { items, active, .. }) in pending.into_iter().flatten() {
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

/// Frames queued for the single stdin writer. The writer drains one frame at a
/// time, so this only absorbs bursts from concurrent invocations.
const WRITE_QUEUE_FRAMES: usize = 32;

impl PluginWorker {
    pub fn new(plugin: &PluginEntry, executable: PathBuf, loader: PluginDefinitionLoader) -> Self {
        let plugin_id = plugin.manifest.id.clone();
        Self {
            inner: Arc::new(PluginWorkerInner {
                host: HostContext::new(&plugin_id, &plugin.manifest.permissions),
                plugin_id,
                executable,
                entry: plugin.entry.clone(),
                loader,
                process: Mutex::new(None),
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
        match tokio::time::timeout(INVOCATION_TIMEOUT, async {
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
            Ok(result) => result,
            Err(_) => Err(Error::Provider(format!(
                "plugin '{}' invocation timed out",
                self.inner.plugin_id
            ))),
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
        let (writer, pending) = self.connection().await?;
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
                id: &request.id,
                method,
                params: &params,
            })?;
            #[cfg(test)]
            if method == "provider.invoke" {
                *LAST_INVOKE_FRAME.lock() = bytes.clone();
            }
            bytes
        };
        let request_id = request.id.clone();
        // Two tasks so the caller can drain (and a cancellation can land) before
        // the request frame is written; neither task lets a wedged child's stdin
        // queue block local teardown.
        let send_cancellation = request_cancellation.clone();
        tokio::spawn({
            let writer = writer.clone();
            async move {
                tokio::select! {
                    () = send_cancellation.cancelled() => {}
                    result = writer.send(request_bytes) => {
                        if result.is_err() {
                            tracing::warn!("plugin worker stopped before its request was sent");
                        }
                    }
                }
            }
        });
        tokio::spawn(async move {
            tokio::select! {
                // Local teardown first: request-cancel, deregistration and the
                // error to the consumer never depend on the child still reading.
                () = cancellation.cancelled() => {
                    request_cancellation.cancel();
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

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "shutdown is only exercised by the worker tests")
    )]
    pub async fn stop(&self) {
        self.inner.process.lock().await.take();
    }

    #[cfg(test)]
    pub(crate) fn sdk_projection_count(&self) -> usize {
        self.inner.host.sdk_projection_count()
    }

    async fn connection(&self) -> Result<(Writer, Arc<Pending>)> {
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
        Ok((process.writer.clone(), Arc::clone(&process.pending)))
    }

    fn spawn(&self) -> Result<WorkerProcess> {
        let entry_url = file_url(&self.inner.entry)?;
        let mut command = self.inner.loader.bun_command(&self.inner.executable);
        command
            .arg(self.inner.loader.worker_path())
            .arg(entry_url.as_str())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|source| {
            Error::context(
                format!(
                    "cannot start plugin worker {}",
                    self.inner.executable.display()
                ),
                source,
            )
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Config("cannot open plugin worker stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Config("cannot open plugin worker stdout".into()))?;
        if let Some(stderr) = child.stderr.take() {
            let plugin_id = self.inner.plugin_id.clone();
            tokio::spawn(async move {
                let mut lines = LineReader::new(stderr);
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            let line = String::from_utf8_lossy(&line);
                            tracing::warn!(plugin = %plugin_id, "{line}");
                        }
                        Ok(None) => break,
                        Err(LineError::TooLong) => {
                            tracing::warn!(
                                plugin = %plugin_id,
                                limit = MAX_LINE_BYTES,
                                "plugin worker stderr line exceeded the limit; truncating the rest"
                            );
                        }
                        Err(LineError::Io(error)) => {
                            tracing::debug!(plugin = %plugin_id, %error, "reading plugin stderr failed");
                            break;
                        }
                    }
                }
            });
        }
        let pending = Arc::new(Pending::open());
        let (writer, messages) = mpsc::channel(WRITE_QUEUE_FRAMES);
        let task = tokio::spawn({
            let plugin_id = self.inner.plugin_id.clone();
            let writer = writer.clone();
            let pending = Arc::clone(&pending);
            let host = self.inner.host.clone();
            async move {
                tokio::select! {
                    () = read_stdout(stdout, &writer, &pending, &host) => {},
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
    let mut lines = LineReader::new(stdout);
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(LineError::TooLong) => {
                // A worker that emits an unterminated line larger than the limit
                // cannot be trusted to recover, so this generation is stopped.
                tracing::warn!(
                    plugin = host.plugin_id(),
                    limit = MAX_LINE_BYTES,
                    "plugin worker wrote an oversized protocol line; stopping it"
                );
                return;
            }
            Err(LineError::Io(error)) => {
                tracing::warn!(plugin = host.plugin_id(), %error, "reading plugin stdout failed");
                break;
            }
        };
        let Ok(message) = serde_json::from_slice::<WorkerMessage>(&line) else {
            tracing::warn!(
                plugin = host.plugin_id(),
                "plugin worker wrote an invalid message"
            );
            continue;
        };
        match message {
            WorkerMessage::Result { id, result, error } => {
                deliver_result(pending, &id, result, error.as_ref());
            }
            WorkerMessage::Event { id, event } => {
                deliver_event(pending, host, &id, &event);
            }
            WorkerMessage::HostCall {
                id,
                request_id,
                method,
                params,
            } => {
                spawn_host_call(writer, pending, host, id, &request_id, method, params);
            }
        }
    }
}

/// Delivers a terminal `Result` frame, waking the consumer with the invocation's
/// own failure message when the plugin reported an error.
fn deliver_result(pending: &Pending, id: &str, result: serde_json::Value, error: Option<&String>) {
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
    let _ = sender.try_send(WorkerStreamItem::Result(Err(Error::Provider(
        "plugin event queue overflowed; the invocation was ended".into(),
    ))));
}

/// Spawns the task that runs one `host_call`, holding an invocation slot.
fn spawn_host_call(
    writer: &Writer,
    pending: &Pending,
    host: &HostContext,
    id: String,
    request_id: &str,
    method: String,
    params: serde_json::Value,
) {
    let Some((invocation, mut active, budget)) = pending.authorization(request_id) else {
        let _ = send_host_error(writer, &id, "plugin invocation is no longer active", None);
        return;
    };
    if !budget.try_acquire_host_call() {
        let _ = send_host_error(writer, &id, "too many concurrent host calls", None);
        return;
    }
    let host = host.clone();
    let writer = writer.clone();
    tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = active.changed() => {
                let _ = send_host_error(
                    &writer,
                    &id,
                    "plugin invocation is no longer active",
                    None,
                );
            }
            () = async {
                let response = host_call_response(&host, &invocation, &method, params, &id).await;
                if let Ok(bytes) = response
                    && writer.try_send(bytes).is_err()
                {
                    // A full stdin queue means the child stopped reading; report
                    // it rather than block the call.
                    tracing::warn!("plugin stdin queue is full; dropped a host-call result");
                }
            } => {},
        }
        budget.release_host_call();
    });
}

/// Turns one `host_call` outcome into the frame sent back over stdin.
async fn host_call_response(
    host: &HostContext,
    invocation: &InvocationState,
    method: &str,
    params: serde_json::Value,
    id: &str,
) -> Result<Vec<u8>> {
    match host.call(invocation, method, params).await {
        Ok(result) => encode_message(&HostMessage::HostResult {
            id,
            result: &result,
        }),
        Err(error) => {
            let (message, status) = match error {
                Error::HttpStatus { status, body } => {
                    (format!("HTTP {status}: {body}"), Some(status))
                }
                Error::Cancelled => ("run was cancelled".into(), None),
                error => (error.to_string(), None),
            };
            encode_message(&HostMessage::HostError {
                id,
                error: &message,
                status,
            })
        }
    }
}

/// Best-effort `HostError` frame; a full stdin queue drops it rather than
/// blocking the reader loop.
fn send_host_error(writer: &Writer, id: &str, error: &str, status: Option<u16>) -> Option<()> {
    let bytes = encode_message(&HostMessage::HostError { id, error, status }).ok()?;
    writer.try_send(bytes).ok()
}

/// The outcome of one [`LineReader::next_line`] call.
enum LineError {
    /// The line exceeded [`MAX_LINE_BYTES`] before a newline or EOF.
    TooLong,
    /// The underlying stream reported an error.
    Io(std::io::Error),
}

/// Reads newline-terminated frames from a worker's stdout/stderr without
/// buffering an entire unterminated line.
///
/// `BufReader::lines` waits for a newline or EOF before yielding anything, so a
/// process that keeps writing without one grows the buffer without limit. This
/// reader checks the accumulated length as it goes and reports
/// [`LineError::TooLong`] once a frame passes [`MAX_LINE_BYTES`].
struct LineReader<R> {
    reader: R,
    buffer: Vec<u8>,
}

impl<R: tokio::io::AsyncRead + Unpin> LineReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: Vec::new(),
        }
    }

    /// Returns the next line without its terminator, `None` at EOF, or an error.
    async fn next_line(&mut self) -> std::result::Result<Option<Vec<u8>>, LineError> {
        use tokio::io::AsyncReadExt;
        loop {
            if let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let mut line: Vec<u8> = self.buffer.drain(..=position).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(line));
            }
            if self.buffer.len() > MAX_LINE_BYTES {
                return Err(LineError::TooLong);
            }
            let mut chunk = [0u8; 8 * 1024];
            let read = self.reader.read(&mut chunk).await.map_err(LineError::Io)?;
            if read == 0 {
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(std::mem::take(&mut self.buffer)));
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

/// Removes one invocation from the registry and tells its consumer it was
/// cancelled. Runs locally, independent of the child process.
fn deregister(pending: &Pending, request_id: &str) {
    if let Some(PendingInvocation { items, active, .. }) = pending.remove(request_id) {
        drop(active);
        let _ = items.try_send(WorkerStreamItem::Result(Err(Error::Cancelled)));
    }
}

/// Best-effort delivery of a `Cancel` frame. It must never block the teardown
/// path: if the child stopped reading stdin, the frame is dropped, which is
/// harmless because the invocation is already deregistered locally.
fn deliver_cancel(writer: &Writer, request_id: &str) {
    let Ok(bytes) = encode_message(&HostMessage::Cancel { id: request_id }) else {
        return;
    };
    match writer.try_send(bytes) {
        Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::debug!(
                "plugin stdin queue is full; dropping the cancel frame for a deregistered invocation"
            );
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
    use super::{super::find_bun, *};

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

    async fn echo_worker(
        root: &std::path::Path,
        source: &str,
    ) -> (
        super::super::definition::PluginDefinitionLoader,
        PluginWorker,
    ) {
        let executable = find_bun().expect("install Bun on PATH to run plugin worker tests");
        let plugin_dir = root.join("echo");
        std::fs::create_dir_all(plugin_dir.join("assets")).unwrap();
        std::fs::write(
            plugin_dir.join("assets/icon.svg"),
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8"/></svg>"#,
        )
        .unwrap();
        std::fs::write(
            plugin_dir.join("plugin.json"),
            r#"{
              "apiVersion": 1,
              "id": "dev.cursorbyoknext.plugins.echo",
              "name": "Echo",
              "version": "0.1.0",
              "icon": "assets/icon.svg",
              "entry": "main.ts",
              "permissions": { "network": [], "resourceOrigin": false }
            }"#,
        )
        .unwrap();
        std::fs::write(plugin_dir.join("main.ts"), source).unwrap();
        let loader =
            super::super::definition::PluginDefinitionLoader::in_temporary_workspace(root).unwrap();
        let entry = plugin_dir.join("main.ts").canonicalize().unwrap();
        let definition = loader.load(&executable, &entry).await.unwrap();
        let providers = super::super::catalog::validate_definition(
            "dev.cursorbyoknext.plugins.echo",
            &definition,
        )
        .unwrap();
        let plugin = super::super::catalog::PluginEntry {
            entry,
            manifest: serde_json::from_slice(
                &std::fs::read(plugin_dir.join("plugin.json")).unwrap(),
            )
            .unwrap(),
            definition,
            providers,
            icon: String::new(),
        };
        (
            loader.clone(),
            PluginWorker::new(&plugin, executable, loader),
        )
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
        assert!(
            find_bun().is_some(),
            "install Bun on PATH to run plugin worker tests"
        );
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
        assert!(
            find_bun().is_some(),
            "install Bun on PATH to run plugin worker tests"
        );
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
        assert!(
            find_bun().is_some(),
            "install Bun on PATH to run plugin worker tests"
        );
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

    #[tokio::test]
    async fn line_reader_splits_frames_and_rejects_an_oversized_line() {
        use tokio::io::AsyncWriteExt;

        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            writer.write_all(b"first\nsecond\r\n").await.unwrap();
            writer
                .write_all(&vec![b'x'; MAX_LINE_BYTES + 8])
                .await
                .unwrap();
        });
        let mut lines = LineReader::new(reader);
        assert_eq!(lines.next_line().await.ok().unwrap().unwrap(), b"first");
        assert_eq!(lines.next_line().await.ok().unwrap().unwrap(), b"second");
        assert!(matches!(lines.next_line().await, Err(LineError::TooLong)));
    }

    #[tokio::test]
    async fn event_overflow_ends_the_invocation_instead_of_dropping_events() {
        let pending = Arc::new(Pending::open());
        let Registration {
            guard,
            mut receiver,
            ..
        } = pending.register(invocation(HashSet::new())).unwrap();
        let sender = pending.sender(&guard.id).unwrap();
        for _ in 0..EVENT_QUEUE_ITEMS {
            sender
                .try_send(WorkerStreamItem::Event(ModelEvent::TextStart))
                .unwrap();
        }
        // One past the bound: the queue must refuse rather than grow.
        assert!(
            sender
                .try_send(WorkerStreamItem::Event(ModelEvent::TextStart))
                .is_err(),
            "the event queue must be bounded"
        );
        // A full queue still delivers the terminal error once a slot is free.
        let _ = receiver.try_recv();
        sender
            .try_send(WorkerStreamItem::Result(Err(Error::Provider(
                "overflow".into(),
            ))))
            .unwrap();
        let mut saw_result = false;
        while let Ok(item) = receiver.try_recv() {
            if matches!(item, WorkerStreamItem::Result(_)) {
                saw_result = true;
            }
        }
        assert!(saw_result);
        drop(guard);
    }
}
