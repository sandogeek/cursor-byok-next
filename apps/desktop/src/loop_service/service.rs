//! Owns one client-loop inference stream per Cursor request ID.

use std::{collections::HashMap, sync::Arc};

use bytes::Bytes;
use futures_util::StreamExt;
use parking_lot::Mutex;
use prost::Message;
use tokio::{
    sync::{Mutex as AsyncMutex, Notify, mpsc},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

use crate::{
    Error, Result,
    backend::KnowledgeService,
    calls::{CallRecorder, CallStore, NewLlmCall, plugin_request_url},
    loop_service::{
        output::OutputHub,
        overflow, project,
        subagent_models::{self, ByokModel},
    },
    model::{ModelInvocation, ModelLatency, Usage},
    plugins::{ADAPTER_ID_PREFIX, PluginHost, PluginModelDescriptor},
    protocol::{
        connect,
        proto::{agent::v1 as agent, inference::v1 as pb},
    },
    provider::ModelEvent,
};

const SUBAGENT_FORCE_POLICY_NONE: &str = "none";
const SUMMARY_UNUSED_WINDOW_PERCENT: f64 = 0.1;
const DEFAULT_SUMMARY_PROMPT_CHARS: u64 = 2_800_000;
const SUMMARY_PROMPT_SCALE_WINDOW: u64 = 1_000_000;
const DISCONNECTED_RUN_GRACE: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(Clone)]
pub(crate) struct InferenceService {
    state: Arc<Mutex<HashMap<String, InferenceRoute>>>,
    changed: Arc<Notify>,
    plugins: PluginHost,
    knowledge: KnowledgeService,
    calls: CallStore,
}

#[derive(Clone)]
pub(crate) enum InferenceRoute {
    Local(Arc<Run>),
    Upstream,
}

/// One client-loop inference run: its input channel, output hub, sequence counter, subscriptions and cancellation.
pub(crate) struct Run {
    input: mpsc::Sender<pb::RunInferenceClientMessage>,
    output: Arc<OutputHub>,
    next_seqno: AsyncMutex<i64>,
    subscriptions: Mutex<Subscriptions>,
    cancellation: CancellationToken,
}

#[derive(Default)]
struct Subscriptions {
    active: usize,
    generation: u64,
}

/// Keeps a run alive while a client stream is attached; dropping the last one starts the disconnect grace timer.
pub(crate) struct Subscription {
    service: InferenceService,
    request_id: String,
    run: Arc<Run>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut subscriptions = self.run.subscriptions.lock();
        subscriptions.active -= 1;
        if subscriptions.active != 0 {
            return;
        }
        let generation = subscriptions.generation;
        let state = Arc::downgrade(&self.service.state);
        let run = Arc::downgrade(&self.run);
        // `Drop` hands the guard the id by move so no copy of the key is made.
        let request_id = std::mem::take(&mut self.request_id);
        tokio::spawn(async move {
            tokio::time::sleep(DISCONNECTED_RUN_GRACE).await;
            let (Some(state), Some(run)) = (state.upgrade(), run.upgrade()) else {
                return;
            };
            let expired = {
                let subscriptions = run.subscriptions.lock();
                subscriptions.active == 0 && subscriptions.generation == generation
            };
            if !expired {
                return;
            }
            run.cancellation.cancel();
            run.output.close();
            let mut routes = state.lock();
            if matches!(routes.get(&request_id), Some(InferenceRoute::Local(current)) if Arc::ptr_eq(current, &run))
            {
                routes.remove(&request_id);
            }
        });
    }
}

/// The plugin model a run is pinned to, as the run itself needs it.
#[derive(Debug)]
struct LocalModel {
    display_name: String,
    vendor: String,
}

struct ServeContext {
    output: Arc<OutputHub>,
    plugins: PluginHost,
    calls: CallStore,
    run_id: String,
    conversation_id: String,
    model_id: String,
    model: Option<LocalModel>,
    knowledge: KnowledgeService,
}

impl InferenceService {
    /// The service over the plugin host, the rule store and the call log.
    pub fn new(plugins: PluginHost, knowledge: KnowledgeService, calls: CallStore) -> Self {
        Self {
            state: Arc::new(Mutex::new(HashMap::new())),
            changed: Arc::new(Notify::new()),
            plugins,
            knowledge,
            calls,
        }
    }

    /// The route recorded for `request_id`, if any.
    pub fn route(&self, request_id: &str) -> Option<InferenceRoute> {
        self.state.lock().get(request_id).cloned()
    }

    /// Waits until `request_id` has a route, which is how the SSE handler learns that `BidiAppend` started a run.
    pub async fn wait_route(&self, request_id: &str) -> InferenceRoute {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(route) = self.route(request_id) {
                return route;
            }
            changed.await;
        }
    }

    /// Records the route for a run's first message, starting a local run when
    /// the model is a plugin model.
    ///
    /// # Errors
    ///
    /// Fails when `message` does not start a run, when the model id or
    /// conversation id is missing, or when the plugin host cannot resolve the
    /// model.
    pub async fn begin(
        &self,
        request_id: &str,
        message: &pb::RunInferenceClientMessage,
    ) -> Result<InferenceRoute> {
        let Some(pb::run_inference_client_message::Message::RunRequest(request)) =
            message.message.as_ref()
        else {
            return Err(Error::Protocol(
                "first RunInference message must start a run".into(),
            ));
        };
        let model_id = request
            .requested_model
            .as_ref()
            .map(|model| model.model_id.as_str())
            .filter(|model| !model.is_empty())
            .ok_or_else(|| Error::Protocol("RunInference model is required".into()))?;
        if request.conversation_id.is_empty() {
            return Err(Error::Protocol(
                "RunInference conversation ID is required".into(),
            ));
        }
        let configured = self.plugins.model(model_id).await?;
        let (model_id, configured) = match configured {
            Some(config) => (model_id.to_string(), Some(config)),
            None => match self.resolve_by_display_name(model_id).await? {
                Some(config) => {
                    tracing::info!(
                        request_id,
                        requested = model_id,
                        resolved = %config.id,
                        "resolved BYOK subagent model by display name"
                    );
                    (config.id.clone(), Some(config))
                }
                None => (model_id.to_string(), None),
            },
        };
        let model_id = model_id.as_str();
        let local = configured.is_some() || model_id.starts_with(ADAPTER_ID_PREFIX);
        let route = if local {
            tracing::info!(
                request_id,
                model_id,
                "routing Cursor RunInference to BYOK provider"
            );
            let (sender, receiver) = mpsc::channel(64);
            let output = Arc::new(OutputHub::new());
            let run = Arc::new(Run {
                input: sender,
                output: Arc::clone(&output),
                next_seqno: AsyncMutex::new(0),
                subscriptions: Mutex::default(),
                cancellation: CancellationToken::new(),
            });
            tokio::spawn(serve(
                ServeContext {
                    output,
                    plugins: self.plugins.clone(),
                    calls: self.calls.clone(),
                    run_id: request_id.into(),
                    conversation_id: request.conversation_id.clone(),
                    model_id: model_id.into(),
                    model: configured.map(|model| LocalModel {
                        display_name: model.display_name,
                        vendor: model.provider_type,
                    }),
                    knowledge: self.knowledge.clone(),
                },
                receiver,
                run.cancellation.clone(),
            ));
            InferenceRoute::Local(Arc::clone(&run))
        } else {
            tracing::info!(
                request_id,
                model_id,
                "routing Cursor RunInference to Cursor upstream"
            );
            InferenceRoute::Upstream
        };
        let mut state = self.state.lock();
        let route = state.entry(request_id.to_owned()).or_insert(route).clone();
        drop(state);
        self.changed.notify_waiters();
        Ok(route)
    }

    /// Attaches a client stream to `run`, replaying output already emitted.
    pub fn subscribe(
        &self,
        request_id: String,
        run: &Arc<Run>,
    ) -> (mpsc::UnboundedReceiver<Bytes>, Subscription) {
        let mut subscriptions = run.subscriptions.lock();
        subscriptions.active += 1;
        subscriptions.generation += 1;
        drop(subscriptions);
        (
            run.output.subscribe(),
            Subscription {
                service: self.clone(),
                request_id,
                run: Arc::clone(run),
            },
        )
    }

    /// Forgets a request id once its client said `FinishRun`.
    pub fn finish(&self, request_id: &str) {
        self.state.lock().remove(request_id);
    }

    async fn resolve_by_display_name(
        &self,
        requested: &str,
    ) -> Result<Option<PluginModelDescriptor>> {
        let models = self.plugins.configured_models().await;
        let hash = {
            let byok = byok_models(&models);
            match subagent_models::resolve(&byok, requested) {
                Ok(hash) => hash.to_owned(),
                Err(_) => return Ok(None),
            }
        };
        Ok(models.into_iter().find(|model| model.id == hash))
    }
}

/// The borrowed view [`subagent_models::resolve`] needs, so resolving a Task
/// model clones no descriptors.
fn byok_models(models: &[PluginModelDescriptor]) -> Vec<ByokModel<'_>> {
    models
        .iter()
        .map(|model| ByokModel {
            hash: &model.id,
            model_id: &model.model_id,
            display_name: &model.display_name,
        })
        .collect()
}

impl Run {
    /// Appends one client message in sequence order.
    ///
    /// The sequence number is checked and advanced around the send, so two
    /// appends cannot observe the same `next` value. The lock is released once
    /// the send completes: nothing after it touches the counter.
    pub async fn append(&self, seqno: i64, message: pb::RunInferenceClientMessage) -> Result<()> {
        let mut next = self.next_seqno.lock().await;
        if seqno < *next {
            return Ok(());
        }
        if seqno != *next {
            return Err(Error::Protocol(format!(
                "RunInference append sequence {seqno} differs from expected {}",
                *next
            )));
        }
        let sent = self.input.send(message).await;
        // Advance even when the receiver is gone: the sequence stays consistent
        // with what the client believes it appended.
        *next += 1;
        sent.map_err(|_| Error::Protocol("RunInference stream has ended".into()))
    }
}

async fn serve(
    context: ServeContext,
    mut input: mpsc::Receiver<pb::RunInferenceClientMessage>,
    cancellation: CancellationToken,
) {
    let ServeContext {
        output,
        plugins,
        calls,
        run_id,
        conversation_id,
        model_id,
        model,
        knowledge,
    } = context;
    let models = plugins.configured_models().await;
    let run_setup = RunSetup {
        run_id: &run_id,
        conversation_id: &conversation_id,
        model_id: &model_id,
        models: &models,
        model: model.as_ref(),
    };
    let mut loop_state = LoopState {
        output: Arc::clone(&output),
        cancellation,
        invocations: Arc::new(Mutex::new(HashMap::new())),
        tasks: JoinSet::new(),
        exit: None,
        provider_call_index: 0,
        // A plugin model carries no context-window figure: the descriptor the
        // provider reports is the only source, and the run learns it from the
        // client's own requested model instead.
        configured_context_window: None,
        run_context_window: None,
    };
    {
        use pb::run_inference_client_message::Message;
        loop {
            let message = tokio::select! {
                () = loop_state.cancellation.cancelled() => {
                    loop_state.exit = Some(RunExit::Disconnected);
                    break;
                }
                message = input.recv() => message,
            };
            let Some(message) = message else {
                break;
            };
            match message.message {
                Some(Message::RunRequest(request)) => {
                    let ready = loop_state.run_ready_message(
                        &run_setup,
                        &request,
                        knowledge.local_cursor_rules().await,
                    );
                    emit(&output, ready);
                }
                Some(Message::InvokeModel(invoke)) => {
                    loop_state
                        .start_invocation(&run_setup, &plugins, &calls, &models, invoke)
                        .await;
                }
                Some(Message::CancelInvocation(cancel)) => {
                    loop_state.cancel_invocation(&cancel.invocation_id);
                }
                Some(Message::FinishRun(finish)) => {
                    loop_state.exit = Some(if finish.aborted {
                        RunExit::Aborted
                    } else {
                        RunExit::Finished
                    });
                    break;
                }
                Some(Message::ResolveSubagentModels(resolve)) => {
                    emit(
                        &output,
                        pb::run_inference_server_message::Message::SubagentModelsResolved(
                            resolve_subagent_models(resolve, &models),
                        ),
                    );
                }
                Some(
                    Message::ClientHeartbeat(_)
                    | Message::AgentEvent(_)
                    | Message::RunRequestLog(_)
                    | Message::ClientFirstCallTimings(_),
                )
                | None => {}
            }
        }
    }
    let exit = loop_state.exit.unwrap_or(RunExit::Disconnected);
    if exit.cancels_invocations() {
        loop_state.cancel_all();
    }
    loop_state.finish(exit).await;
    output.close();
}

/// How a Run's client loop ended, decided inside the loop and acted on once.
///
/// The variants are exclusive: `Finished` keeps live invocations running,
/// `Aborted` cancels them, and `Disconnected` closes the stream without an
/// end-of-stream marker the client can no longer receive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunExit {
    /// The client sent `FinishRun` and asked for a normal finish.
    Finished,
    /// The client sent `FinishRun` with `aborted`.
    Aborted,
    /// The client's stream went away before `FinishRun`.
    Disconnected,
}

impl RunExit {
    /// Whether the run's own invocations must be cancelled on the way out.
    fn cancels_invocations(self) -> bool {
        matches!(self, Self::Aborted | Self::Disconnected)
    }

    /// Whether the downstream stream still needs the end-of-stream marker.
    fn sends_end_stream(self) -> bool {
        self != Self::Disconnected
    }
}

/// Per-stream state owned by [`serve`]: the output hub, live invocations,
/// spawned tasks, and the context window the client asked for.
struct LoopState {
    output: Arc<OutputHub>,
    cancellation: CancellationToken,
    invocations: Arc<Mutex<HashMap<String, CancellationToken>>>,
    tasks: JoinSet<()>,
    exit: Option<RunExit>,
    provider_call_index: u64,
    configured_context_window: Option<u64>,
    run_context_window: Option<u64>,
}

impl LoopState {
    fn run_ready_message(
        &mut self,
        setup: &RunSetup<'_>,
        request: &pb::RunInferenceRunRequest,
        rules: Vec<agent::CursorRule>,
    ) -> pb::run_inference_server_message::Message {
        let requested_window = project::requested_context_window(request.requested_model.as_ref())
            .ok()
            .flatten();
        self.run_context_window = requested_window.or(self.configured_context_window);
        if self.run_context_window.is_none() {
            tracing::info!(
                model_id = setup.model_id,
                "inference_context_window_unknown"
            );
        }
        pb::run_inference_server_message::Message::RunReady(pb::RunInferenceRunReady {
            summarization: Some(summarization_config(request, self.run_context_window)),
            prompt_model_metadata: Some(
                setup.prompt_model_metadata(request, self.run_context_window),
            ),
            non_file_rules: rules,
            ..setup.run_ready_defaults(request)
        })
    }

    async fn start_invocation(
        &mut self,
        setup: &RunSetup<'_>,
        plugins: &PluginHost,
        calls: &CallStore,
        models: &[PluginModelDescriptor],
        invoke: pb::RunInferenceInvokeModel,
    ) {
        let id = invoke.invocation_id.clone();
        let Some(request) = invoke.request else {
            invocation_error(&self.output, id, "RunInference model request is missing");
            return;
        };
        if invoke.delta_base.is_some() || invoke.compressed_request.is_some() {
            invocation_error(
                &self.output,
                id,
                "RunInference delta or compressed requests are not supported",
            );
            return;
        }
        let request =
            match project::project(&request, setup.model_id, self.run_context_window, models) {
                Ok(request) => request,
                Err(error) => {
                    invocation_error(&self.output, id, &error.to_string());
                    return;
                }
            };
        let context_window_tokens = request.model.context_window_tokens;
        let cancellation = self.cancellation.child_token();
        self.invocations
            .lock()
            .insert(id.clone(), cancellation.clone());
        let recorder = start_recorder(setup, calls, &id, self.provider_call_index, &request).await;
        self.tasks.spawn(serve_invocation(
            plugins.clone(),
            Arc::clone(&self.output),
            Arc::clone(&self.invocations),
            cancellation,
            recorder,
            ModelInvocation {
                call_id: id,
                run_id: setup.run_id.to_owned(),
                conversation_id: setup.conversation_id.to_owned(),
                provider_call_index: self.provider_call_index,
                request,
            },
            context_window_tokens,
        ));
        self.provider_call_index += 1;
    }

    fn cancel_invocation(&self, id: &str) {
        if let Some(token) = self.invocations.lock().remove(id) {
            token.cancel();
        }
    }

    fn cancel_all(&self) {
        for (_, token) in self.invocations.lock().drain() {
            token.cancel();
        }
    }

    /// Waits for every invocation, then closes the stream unless the client
    /// already went away.
    async fn finish(mut self, exit: RunExit) {
        while let Some(joined) = self.tasks.join_next().await {
            if let Err(error) = joined
                && !error.is_cancelled()
            {
                tracing::warn!(%error, "a RunInference invocation panicked");
            }
        }
        if exit.sends_end_stream() {
            self.output.emit(connect::encode_end_stream());
        }
    }
}

/// The identity a Run carries into every invocation and into `RunReady`.
#[derive(Clone, Copy, Debug)]
struct RunSetup<'a> {
    run_id: &'a str,
    conversation_id: &'a str,
    model_id: &'a str,
    models: &'a [PluginModelDescriptor],
    model: Option<&'a LocalModel>,
}

impl RunSetup<'_> {
    /// The `RunReady` fields that do not depend on the client's request.
    fn run_ready_defaults(&self, request: &pb::RunInferenceRunRequest) -> pb::RunInferenceRunReady {
        let routed_model_display_name = self
            .model
            .map(|model| model.display_name.as_str())
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        pb::RunInferenceRunReady {
            resolved_model: request.requested_model.clone(),
            supports_self_summary: true,
            routed_model_display_name,
            accepts_aborted_finish: true,
            accepts_subagent_model_resolution: true,
            ..pb::RunInferenceRunReady::default()
        }
    }

    /// Prompt-model metadata, derived from the vendor and display name the
    /// model advertises.
    fn prompt_model_metadata(
        &self,
        request: &pb::RunInferenceRunRequest,
        context_window_tokens: Option<u64>,
    ) -> pb::RunInferencePromptModelMetadata {
        let (display_name, vendor) = self.model.map_or(("", "openai"), |model| {
            (model.display_name.as_str(), model.vendor.as_str())
        });
        let anthropic = vendor.eq_ignore_ascii_case("anthropic")
            || vendor.eq_ignore_ascii_case("messages")
            || display_name.to_ascii_lowercase().contains("claude");
        let name = display_name.to_ascii_lowercase();
        let claude = anthropic && name.contains("claude");
        let gpt5 = !anthropic && name.starts_with("gpt-5");
        pb::RunInferencePromptModelMetadata {
            vendor: if anthropic { "anthropic" } else { "openai" }.into(),
            prompt_version: "latest".into(),
            is_claude_4x: claude,
            is_sonnet4: claude && name.contains("sonnet"),
            is_gpt5_family: gpt5,
            is_gpt5: gpt5,
            persona: "auto".into(),
            self_identity_name: "Cursor".into(),
            agent_token_limit: context_window_tokens.and_then(i32_tokens),
            agent_mode_configs: agent_mode_configs(),
            ask_question_config: Some(pb::RunInferenceAskQuestionConfig {
                tool_name: "AskQuestion".into(),
                ..Default::default()
            }),
            subagent_model_config: Some(subagent_model_config(self.model_id, self.models, request)),
            ..Default::default()
        }
    }
}

fn summarization_config(
    request: &pb::RunInferenceRunRequest,
    context_window_tokens: Option<u64>,
) -> pb::RunInferenceSummarizationConfig {
    pb::RunInferenceSummarizationConfig {
        model: request.requested_model.clone(),
        is_fallback_to_main_model: true,
        max_prompt_chars: summary_prompt_chars(context_window_tokens),
        background: Some(pb::RunInferenceBackgroundSummarizationConfig {
            unused_percent_tokens_threshold_to_start: Some(SUMMARY_UNUSED_WINDOW_PERCENT),
            unused_percent_tokens_threshold_to_persist: Some(SUMMARY_UNUSED_WINDOW_PERCENT),
            ..Default::default()
        }),
    }
}

fn summary_prompt_chars(context_window_tokens: Option<u64>) -> i32 {
    let window =
        context_window_tokens.filter(|window| *window > 0 && *window < SUMMARY_PROMPT_SCALE_WINDOW);
    let chars = window.map_or(DEFAULT_SUMMARY_PROMPT_CHARS, |window| {
        DEFAULT_SUMMARY_PROMPT_CHARS.saturating_mul(window) / SUMMARY_PROMPT_SCALE_WINDOW
    });
    count_i32(chars)
}

fn overflow_response(message: &str) -> Option<pb::InferenceStreamResponse> {
    overflow::is_context_overflow(message).then(|| {
        stream_response(pb::inference_stream_response::Response::Error(
            pb::InferenceStreamError {
                message: message.into(),
                code: "context_length_exceeded".into(),
                is_input_token_limit_error: true,
                error_type: pb::InferenceStreamErrorType::InputTokenLimit.into(),
                ..Default::default()
            },
        ))
    })
}

fn i32_tokens(tokens: u64) -> Option<i32> {
    i32::try_from(tokens).ok()
}

fn subagent_model_config(
    parent_model: &str,
    models: &[PluginModelDescriptor],
    request: &pb::RunInferenceRunRequest,
) -> pb::RunInferenceSubagentModelConfig {
    use pb::run_inference_subagent_model_override::Selection;
    let mut seen = std::collections::HashSet::new();
    let slugs = models
        .iter()
        .map(|m| m.id.as_str())
        .chain(
            request
                .selected_subagent_models
                .iter()
                .map(|m| m.model_id.as_str()),
        )
        .filter(|slug| !slug.is_empty() && *slug != "default" && seen.insert(*slug));
    let overrides = request
        .subagent_model_overrides
        .iter()
        .filter_map(|entry| {
            let (kind, model_id) = match entry.selection.as_ref()? {
                Selection::Model(m) if m.model_id == "default" => ("inherit", None),
                Selection::Model(m) if !m.model_id.is_empty() => {
                    ("model", Some(m.model_id.clone()))
                }
                Selection::Inherit(true) => ("inherit", None),
                Selection::Disabled(true) => ("disabled", None),
                _ => return None,
            };
            Some(pb::RunInferenceResolvedSubagentModelOverride {
                subagent_type: entry.subagent_type.clone(),
                kind: kind.into(),
                model_id,
            })
        })
        .collect();
    pb::RunInferenceSubagentModelConfig {
        force_policy: SUBAGENT_FORCE_POLICY_NONE.into(),
        force_model_id: None,
        parent_max_mode: false,
        models: slugs
            .map(|slug| pb::RunInferenceSubagentModelPolicy {
                slug: slug.into(),
                advertised: true,
                valid: true,
                blocked: false,
                requires_max_mode: false,
                representative_cost_dollars: 0.0,
                legacy_aliases: Vec::new(),
                turned_off_by_user: false,
            })
            .collect(),
        overrides,
        task_tool_omitted: false,
        parent_requested_model_name: parent_model.into(),
    }
}

fn resolve_subagent_models(
    resolve: pb::RunInferenceResolveSubagentModels,
    models: &[PluginModelDescriptor],
) -> pb::RunInferenceSubagentModelsResolved {
    let byok = byok_models(models);
    let resolved = resolve
        .requested_models
        .iter()
        .map(|requested| {
            let hash = if byok.iter().any(|model| model.hash == requested) {
                Some(requested.clone())
            } else {
                subagent_models::resolve(&byok, requested)
                    .ok()
                    .map(str::to_owned)
                    .filter(|hash| byok.iter().any(|model| model.hash == hash))
            };
            pb::RunInferenceResolvedSubagentModel {
                requested: requested.clone(),
                resolved_slug: hash,
                policy: None,
            }
        })
        .collect();
    pb::RunInferenceSubagentModelsResolved {
        resolution_id: resolve.resolution_id,
        models: resolved,
    }
}

async fn serve_invocation(
    plugins: PluginHost,
    output: Arc<OutputHub>,
    invocations: Arc<Mutex<HashMap<String, CancellationToken>>>,
    cancellation: CancellationToken,
    recorder: CallRecorder,
    invocation: ModelInvocation,
    context_window_tokens: Option<u64>,
) {
    let id = invocation.call_id.clone();
    let _cancel_on_drop = recorder.cancel_on_drop();
    let mut stream = plugins.stream_model(invocation, cancellation.clone(), Some(recorder.clone()));
    let mut tool_calls = HashMap::<usize, (String, String, String)>::new();
    let mut error = None;
    // One invocation response per provider event except usage, which sends a
    // pair; the sender is captured once so a stream of deltas allocates nothing.
    let mut send = |response: pb::InferenceStreamResponse| {
        emit(
            &output,
            pb::run_inference_server_message::Message::InvocationResponse(
                pb::RunInferenceInvocationResponse {
                    invocation_id: id.clone(),
                    response: Some(response),
                },
            ),
        );
    };
    while let Some(next) = tokio::select! {
        () = cancellation.cancelled() => None,
        next = stream.next() => next,
    } {
        match next {
            Ok(event) => {
                recorder.event(&event).await;
                model_responses(event, &mut tool_calls, context_window_tokens, &mut send);
            }
            Err(value) => {
                let message = value.to_string();
                if let Some(response) = overflow_response(&message) {
                    send(response);
                }
                recorder.failed(&value).await;
                error = Some(message);
                break;
            }
        }
    }
    if !recorder.is_finished() {
        if cancellation.is_cancelled() {
            recorder.cancelled().await;
        } else {
            let ended = Error::Provider("provider stream ended without Done".into());
            recorder.failed(&ended).await;
        }
    }
    invocations.lock().remove(&id);
    let message = pb::RunInferenceInvocationEnd {
        invocation_id: id,
        error: error.map(|message| pb::RunInferenceInvocationError {
            code: 13,
            message,
            ..Default::default()
        }),
        ..Default::default()
    };
    emit(
        &output,
        pb::run_inference_server_message::Message::InvocationEnd(message),
    );
}

/// Projects one provider event onto the invocation responses it produces.
///
/// Every event but usage maps to at most one response, so the responses are
/// handed to `send` instead of collected.
fn model_responses(
    event: ModelEvent,
    tool_calls: &mut HashMap<usize, (String, String, String)>,
    context_window_tokens: Option<u64>,
    send: &mut impl FnMut(pb::InferenceStreamResponse),
) {
    use pb::inference_stream_response::Response;
    let response = match event {
        ModelEvent::Usage(usage) => {
            for response in usage_responses(usage, context_window_tokens) {
                send(response);
            }
            return;
        }
        ModelEvent::TextDelta(text) => Response::TextPart(pb::InferenceTextStreamPart {
            text,
            is_final: false,
        }),
        ModelEvent::TextEnd => Response::TextPart(pb::InferenceTextStreamPart {
            text: String::new(),
            is_final: true,
        }),
        ModelEvent::ThinkingDelta(text) => {
            Response::ThinkingPart(pb::InferenceThinkingStreamPart {
                text,
                is_final: false,
                ..Default::default()
            })
        }
        ModelEvent::ThinkingEnd => Response::ThinkingPart(pb::InferenceThinkingStreamPart {
            text: String::new(),
            is_final: true,
            ..Default::default()
        }),
        ModelEvent::ThinkingSignature(signature) => {
            Response::ThinkingPart(pb::InferenceThinkingStreamPart {
                text: String::new(),
                signature: Some(signature),
                is_final: true,
                ..Default::default()
            })
        }
        ModelEvent::RedactedThinking(data) => {
            Response::ThinkingPart(pb::InferenceThinkingStreamPart {
                text: String::new(),
                redacted_data: Some(data),
                is_final: true,
                ..Default::default()
            })
        }
        ModelEvent::ToolCallStart {
            index,
            call_id,
            name,
        } => {
            tool_calls.insert(index, (call_id.clone(), name.clone(), String::new()));
            Response::ToolCallPart(pb::InferenceToolCallStreamPart {
                tool_call_id: call_id,
                tool_name: name,
                args: String::new(),
                is_complete: false,
                tool_index: i32::try_from(index).ok(),
                ..Default::default()
            })
        }
        ModelEvent::ToolCallArgumentsDelta { index, delta } => {
            let Some((call_id, _, arguments)) = tool_calls.get_mut(&index) else {
                return;
            };
            arguments.push_str(&delta);
            Response::ToolCallPart(pb::InferenceToolCallStreamPart {
                tool_call_id: call_id.clone(),
                tool_name: String::new(),
                args: delta,
                is_complete: false,
                tool_index: i32::try_from(index).ok(),
                ..Default::default()
            })
        }
        ModelEvent::ToolCallEnd { index } => {
            let Some((call_id, name, arguments)) = tool_calls.remove(&index) else {
                return;
            };
            Response::ToolCallPart(pb::InferenceToolCallStreamPart {
                tool_call_id: call_id,
                tool_name: name,
                args: arguments,
                is_complete: true,
                tool_index: i32::try_from(index).ok(),
                ..Default::default()
            })
        }
        ModelEvent::Start { model_call_id } => {
            Response::InvocationId(pb::InferenceInvocationIdInfo {
                invocation_id: model_call_id,
            })
        }
        ModelEvent::TextStart
        | ModelEvent::ThinkingStart
        | ModelEvent::ProviderReplayState(_)
        | ModelEvent::Done(_) => return,
    };
    send(stream_response(response));
}

fn usage_responses(
    usage: Usage,
    context_window_tokens: Option<u64>,
) -> Vec<pb::InferenceStreamResponse> {
    use pb::inference_stream_response::Response;
    let prompt_tokens = usage.context_input_tokens.or(usage.input_tokens);
    let completion_tokens = usage.output_tokens;
    let total_tokens = usage
        .total_tokens
        .or_else(|| match (prompt_tokens, completion_tokens) {
            (Some(prompt), Some(completion)) => Some(prompt.saturating_add(completion)),
            (Some(prompt), None) => Some(prompt),
            (None, Some(completion)) => Some(completion),
            (None, None) => None,
        });
    vec![
        stream_response(Response::Usage(pb::InferenceUsageInfo {
            prompt_tokens: count_i32(prompt_tokens.unwrap_or_default()),
            completion_tokens: count_i32(completion_tokens.unwrap_or_default()),
            total_tokens: total_tokens.map(count_i32),
        })),
        stream_response(Response::ExtendedUsage(pb::InferenceExtendedUsageInfo {
            input_tokens: count_i32(usage.input_tokens.unwrap_or_default()),
            output_tokens: count_i32(usage.output_tokens.unwrap_or_default()),
            cache_read_tokens: count_i32(usage.cache_read_tokens.unwrap_or_default()),
            cache_write_tokens: count_i32(usage.cache_write_tokens.unwrap_or_default()),
            max_tokens: count_i32(context_window_tokens.unwrap_or_default()),
            reasoning_tokens: usage.reasoning_tokens.map(count_i32),
        })),
    ]
}

fn stream_response(
    response: pb::inference_stream_response::Response,
) -> pb::InferenceStreamResponse {
    pb::InferenceStreamResponse {
        response: Some(response),
        ..Default::default()
    }
}

fn agent_mode_configs() -> Vec<pb::RunInferenceAgentModeConfig> {
    const MODES: [&str; 6] = ["agent", "chat", "plan", "debug", "triage", "multitask"];
    const SWITCH: [&str; 5] = ["agent", "chat", "plan", "debug", "multitask"];
    MODES
        .into_iter()
        .map(|mode| pb::RunInferenceAgentModeConfig {
            mode_id: mode.into(),
            enable_ask_question_tool: true,
            switch_mode_tool_config: (mode == "agent").then(|| {
                pb::RunInferenceSwitchModeToolConfig {
                    target_modes: SWITCH.into_iter().map(str::to_owned).collect(),
                }
            }),
        })
        .collect()
}

/// A count as Cursor's `i32` field. A count past `i32::MAX` saturates; no real
/// prompt, context window or summary prompt reaches it.
fn count_i32(value: u64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn invocation_error(output: &OutputHub, id: String, message: &str) {
    emit(
        output,
        pb::run_inference_server_message::Message::InvocationEnd(pb::RunInferenceInvocationEnd {
            invocation_id: id,
            error: Some(pb::RunInferenceInvocationError {
                code: 3,
                message: message.into(),
                ..Default::default()
            }),
            ..Default::default()
        }),
    );
}

fn emit(output: &OutputHub, message: pb::run_inference_server_message::Message) {
    let message = pb::RunInferenceServerMessage {
        message: Some(message),
    };
    if let Ok(frame) = connect::encode_message(&message) {
        output.emit(frame);
    }
}

/// Decodes one `BidiAppend` payload into the client message it carries.
pub(crate) fn decode_append(data: &[u8]) -> Result<pb::RunInferenceClientMessage> {
    pb::RunInferenceClientMessage::decode(data).map_err(Error::from)
}

async fn start_recorder(
    setup: &RunSetup<'_>,
    store: &CallStore,
    call_id: &str,
    provider_call_index: u64,
    request: &crate::model::ModelRequest,
) -> CallRecorder {
    let request_url = plugin_request_url(&request.model.model_id);
    let display_name = request
        .model
        .display_name
        .clone()
        .or_else(|| setup.model.map(|model| model.display_name.clone()))
        .unwrap_or_else(|| request_url.model.clone());
    CallRecorder::start(
        store.clone(),
        NewLlmCall {
            call_id: call_id.into(),
            run_id: setup.run_id.into(),
            conversation_id: setup.conversation_id.into(),
            provider_call_index: i64::try_from(provider_call_index).unwrap_or(i64::MAX),
            model_hash: request.model.model_id.clone(),
            provider_type: request_url.provider.clone(),
            provider_url: request_url.url.clone(),
            request_type: request_url.provider.clone(),
            request_url: request_url.url,
            model_id: request_url.model,
            display_name,
            reasoning_effort: request.model.reasoning.effort().map(str::to_owned),
            fast: request.model.latency == ModelLatency::Fast,
            message_count: request.history.len(),
            tool_count: request.prompt.tools.len(),
            detailed: false,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invocation_cancellation_propagates_from_the_run() {
        let run = CancellationToken::new();
        let invocation = run.child_token();
        assert!(!invocation.is_cancelled());
        run.cancel();
        assert!(invocation.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn the_last_subscription_cancels_a_disconnected_run_after_its_grace_period() {
        let root = tempfile::tempdir().unwrap();
        let service = InferenceService::new(
            PluginHost::for_test(root.path()).unwrap(),
            KnowledgeService::with_root(root.path().join("rules")).unwrap(),
            CallStore::at(root.path().join("calls.db")),
        );
        let (sender, _receiver) = mpsc::channel(1);
        let run = Arc::new(Run {
            input: sender,
            output: Arc::new(OutputHub::new()),
            next_seqno: AsyncMutex::new(0),
            subscriptions: Mutex::default(),
            cancellation: CancellationToken::new(),
        });
        service
            .state
            .lock()
            .insert("run".into(), InferenceRoute::Local(Arc::clone(&run)));
        let (_receiver, subscription) = service.subscribe("run".into(), &run);
        drop(subscription);
        tokio::task::yield_now().await;
        assert!(!run.cancellation.is_cancelled());
        let (_receiver, subscription) = service.subscribe("run".into(), &run);
        tokio::time::advance(DISCONNECTED_RUN_GRACE).await;
        tokio::task::yield_now().await;
        assert!(
            !run.cancellation.is_cancelled(),
            "reconnecting must keep the run alive"
        );
        drop(subscription);
        tokio::task::yield_now().await;
        assert!(!run.cancellation.is_cancelled());
        tokio::time::advance(DISCONNECTED_RUN_GRACE).await;
        tokio::task::yield_now().await;
        assert!(run.cancellation.is_cancelled());
        assert!(run.output.is_closed());
        assert!(service.route("run").is_none());
    }

    #[tokio::test]
    async fn official_models_do_not_create_call_records() {
        let root = tempfile::tempdir().unwrap();
        let store = CallStore::at(root.path().join("calls.db"));
        let service = InferenceService::new(
            PluginHost::for_test(root.path()).unwrap(),
            KnowledgeService::with_root(root.path().join("rules")).unwrap(),
            store.clone(),
        );
        let route = service
            .begin(
                "official-run",
                &pb::RunInferenceClientMessage {
                    message: Some(pb::run_inference_client_message::Message::RunRequest(
                        pb::RunInferenceRunRequest {
                            conversation_id: "conversation".into(),
                            requested_model: Some(pb::InferenceRequestedModel {
                                model_id: "gpt-5".into(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    )),
                },
            )
            .await
            .unwrap();
        assert!(matches!(route, InferenceRoute::Upstream));
        let snapshot = store.snapshot(200).await;
        assert_eq!(snapshot.calls, Vec::<crate::calls::LlmCallSummary>::new());
        assert_eq!(snapshot.storage.call_count, 0);
    }

    #[test]
    fn output_replays_until_close_then_releases_its_history() {
        let output = OutputHub::new();
        output.emit(Bytes::from_static(b"first"));
        let mut receiver = output.subscribe();
        assert_eq!(receiver.try_recv().unwrap(), Bytes::from_static(b"first"));
        output.close();
        assert!(output.is_closed());
        assert!(!output.has_history());
        assert!(receiver.try_recv().is_err());
        assert!(!output.emit(Bytes::from_static(b"late")));
    }
}
