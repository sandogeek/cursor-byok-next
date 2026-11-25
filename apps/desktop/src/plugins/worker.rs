//! Runs one thread-confined `QuickJS` VM per active trusted plugin.

use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{
    Error, Result,
    builtin::MAGPIE_PLUGIN_ID,
    host_calls::HostContext,
    invocation::{InvocationState, Pending, WorkerStreamItem},
    manifest::ResourceOriginPolicy,
    vm::{
        self,
        loader::Modules,
        protocol::{HostMessage, WorkerMessage},
    },
    wire,
};
use crate::{calls::CallRecorder, model::ModelInvocation};

const INVOCATION_TIMEOUT: Duration = Duration::from_secs(60);

/// Complete frames in the bounded in-process session queue.
type Writer = std::sync::mpsc::SyncSender<Vec<u8>>;

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

impl PluginWorker {
    pub fn new(
        plugin_id: String,
        resource_origin: ResourceOriginPolicy,
        network_hosts: Vec<String>,
        entry: String,
        loader: Modules,
    ) -> Self {
        Self {
            inner: Arc::new(PluginWorkerInner {
                host: HostContext::new(&plugin_id, resource_origin, network_hosts),
                plugin_id,
                entry,
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
        let mut state = InvocationState::new(
            allowed_origins,
            request_cancellation.clone(),
            recorder,
            model,
        );
        if self.inner.plugin_id == MAGPIE_PLUGIN_ID && method == "models.list" {
            state = state.with_magpie_catalog_tracking();
        }
        let (request, receiver, items, mut active) = pending.register(state)?;
        let request_id = request.id().to_owned();
        let request_bytes = {
            let bytes = encode_message(&HostMessage::Request {
                id: request_id.clone(),
                method: method.to_owned(),
                params: params.clone(),
            })?;
            #[cfg(test)]
            if method == "provider.invoke" {
                *self.inner.last_invoke_frame.lock() = bytes.clone();
            }
            bytes
        };
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
                            pending.complete_result(&id, result, error.as_deref());
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

/// Delivers one streamed event, ending the invocation explicitly if the event
/// cannot be converted or the queue is full.
fn deliver_event(pending: &Pending, host: &HostContext, id: &str, event: &serde_json::Value) {
    let Some(sender) = pending.sender(id) else {
        return;
    };
    let event = match wire::model_event(event) {
        Ok(event) => event,
        Err(error) => {
            pending.remove_and_fail(id, error);
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
    pending.remove_and_fail(
        id,
        Error::Provider("plugin event queue overflowed; the invocation was ended".into()),
    );
}

/// Removes one invocation from the registry and tells its consumer it was
/// cancelled. Runs locally, independent of the VM thread.
fn deregister(pending: &Pending, request_id: &str) {
    pending.remove_and_fail(request_id, Error::Cancelled);
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
    use super::super::invocation::{EVENT_QUEUE_ITEMS, MAX_HOST_CALLS_PER_INVOCATION};
    use super::*;
    use crate::provider::ModelEvent;
    use tokio::io::AsyncWriteExt;

    fn invocation(allowed_origins: HashSet<String>) -> InvocationState {
        InvocationState::new(allowed_origins, CancellationToken::new(), None, None)
    }

    fn host() -> HostContext {
        HostContext::new("test", ResourceOriginPolicy::Allowed, Vec::new())
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
        let missing = super::super::adapters::stream(
            &host,
            &invocation(HashSet::from(["http://127.0.0.1:1".into()])),
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
        registered.set_model(Arc::new(crate::model::ModelInvocation {
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
        let foreign = super::super::adapters::stream(
            &host,
            &registered,
            serde_json::json!({
                "protocol": "openai-chat",
                "url": "http://10.0.0.2/v1/chat/completions",
                "model": "gpt"
            }),
        )
        .await
        .unwrap_err();
        assert!(foreign.to_string().contains("cannot access host"));
        let credentials = super::super::adapters::stream(
            &host,
            &registered,
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
        let invocation = InvocationState::new(
            HashSet::from([origin.clone()]),
            CancellationToken::new(),
            None,
            None,
        )
        .with_magpie_catalog_tracking();
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
        InvocationState::new(
            HashSet::new(),
            CancellationToken::new(),
            Some(recorder),
            None,
        )
    }

    fn recording_host() -> HostContext {
        HostContext::new(
            "test",
            ResourceOriginPolicy::Denied,
            vec!["example.com".into()],
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

    fn echo_worker(_root: &std::path::Path, source: &str) -> (Modules, PluginWorker) {
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
        let manifest: super::super::manifest::PluginManifest = serde_json::from_value(serde_json::json!({"apiVersion":1,"id":"dev.cursorbyoknext.plugins.echo","name":"Echo","version":"0.1.0","icon":"assets/magpie.svg","entry":"echo/main.js","permissions":{"network":[],"resourceOrigin":false}})).unwrap();
        (
            loader,
            PluginWorker::new(
                manifest.id.clone(),
                manifest.resource_origin_policy(),
                manifest.network_hosts(),
                "echo/main.js".into(),
                loader,
            ),
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
        let temp = tempfile::tempdir().unwrap();
        let (_loader, worker) = echo_worker(temp.path(), ECHO_PLUGIN);
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
        let (_loader, worker) = echo_worker(temp.path(), ECHO_PLUGIN);
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
        let (_loader, worker) = echo_worker(temp.path(), source);
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
    fn event_overflow_ends_the_invocation_instead_of_dropping_events() {
        let pending = Arc::new(Pending::open());
        let (guard, mut receiver, items, active) =
            pending.register(invocation(HashSet::new())).unwrap();
        let state = pending.authorization(guard.id()).unwrap().0;
        let event = serde_json::json!({"type":"text-start"});
        for _ in 0..EVENT_QUEUE_ITEMS {
            deliver_event(&pending, &host(), guard.id(), &event);
        }
        assert_eq!(receiver.len(), EVENT_QUEUE_ITEMS);
        deliver_event(&pending, &host(), guard.id(), &event);
        assert!(pending.authorization(guard.id()).is_none());
        assert!(state.is_cancelled());
        drop(state);
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
        let (_, worker) = echo_worker(temp.path(), source);
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
        let (_, worker) = echo_worker(temp.path(), ECHO_PLUGIN);
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
            let (guard, _, _, _) = pending.register(invocation(HashSet::new())).unwrap();
            let id = guard.id().to_owned();
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
        let (_, mut worker) = echo_worker(temp.path(), &source);
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
        let (_, mut worker) = echo_worker(temp.path(), &source);
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
        assert!(!second.is_cancelled());
        first.cancel();
        second.cancel();
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
        let (_, worker) = echo_worker(temp.path(), &source);
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
        let (_, mut worker) = echo_worker(temp.path(), ECHO_PLUGIN);
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
