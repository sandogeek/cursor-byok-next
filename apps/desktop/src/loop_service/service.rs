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
        proto::{agent::v1 as agent, inference as pb},
    },
    provider::ModelEvent,
};

const SUBAGENT_FORCE_POLICY_NONE: &str = "none";
const SUMMARY_UNUSED_WINDOW_PERCENT: f64 = 0.1;
const DEFAULT_SUMMARY_PROMPT_CHARS: i32 = 2_800_000;
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
        let request_id = self.request_id.clone();
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

struct LocalModel {
    display_name: String,
    vendor: String,
    context_window_tokens: Option<u64>,
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
    pub fn new(plugins: PluginHost, knowledge: KnowledgeService, calls: CallStore) -> Self {
        Self {
            state: Arc::new(Mutex::new(HashMap::new())),
            changed: Arc::new(Notify::new()),
            plugins,
            knowledge,
            calls,
        }
    }

    pub fn route(&self, request_id: &str) -> Option<InferenceRoute> {
        self.state.lock().get(request_id).cloned()
    }

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
            let output = Arc::new(OutputHub::default());
            let run = Arc::new(Run {
                input: sender,
                output: output.clone(),
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
                        context_window_tokens: None,
                    }),
                    knowledge: self.knowledge.clone(),
                },
                receiver,
                run.cancellation.clone(),
            ));
            InferenceRoute::Local(run)
        } else {
            tracing::info!(
                request_id,
                model_id,
                "routing Cursor RunInference to Cursor upstream"
            );
            InferenceRoute::Upstream
        };
        let mut state = self.state.lock();
        let route = state
            .entry(request_id.into())
            .or_insert_with(|| route.clone())
            .clone();
        drop(state);
        self.changed.notify_waiters();
        Ok(route)
    }

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
                run: run.clone(),
            },
        )
    }

    pub fn finish(&self, request_id: &str) {
        self.state.lock().remove(request_id);
    }

    async fn resolve_by_display_name(
        &self,
        requested: &str,
    ) -> Result<Option<PluginModelDescriptor>> {
        let models = self.plugins.configured_models().await;
        let byok = models
            .iter()
            .map(|model| ByokModel {
                hash: model.id.clone(),
                model_id: model.model_id.clone(),
                display_name: model.display_name.clone(),
            })
            .collect::<Vec<_>>();
        let Ok(hash) = subagent_models::resolve(&byok, requested) else {
            return Ok(None);
        };
        Ok(models.into_iter().find(|model| model.id == hash))
    }
}

impl Run {
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
        self.input
            .send(message)
            .await
            .map_err(|_| Error::Protocol("RunInference stream has ended".into()))?;
        *next += 1;
        Ok(())
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
    let invocations = Arc::new(Mutex::new(HashMap::<String, CancellationToken>::new()));
    let mut tasks = JoinSet::new();
    let mut finished = false;
    let mut disconnected = false;
    let mut provider_call_index = 0;
    let configured_context_window = model.as_ref().and_then(|model| model.context_window_tokens);
    let mut run_context_window = configured_context_window;
    let models = plugins.configured_models().await;
    loop {
        let message = tokio::select! {
            () = cancellation.cancelled() => {
                disconnected = true;
                break;
            }
            message = input.recv() => message,
        };
        let Some(message) = message else {
            break;
        };
        use pb::run_inference_client_message::Message;
        match message.message {
            Some(Message::RunRequest(request)) => {
                let requested_window =
                    project::requested_context_window(request.requested_model.as_ref())
                        .ok()
                        .flatten();
                run_context_window = requested_window.or(configured_context_window);
                if run_context_window.is_none() {
                    tracing::info!(
                        request_id = %run_id,
                        model_id,
                        "inference_context_window_unknown"
                    );
                }
                let ready = run_ready(
                    &request,
                    model.as_ref(),
                    &models,
                    &model_id,
                    run_context_window,
                    knowledge.local_cursor_rules(),
                );
                emit(
                    &output,
                    pb::run_inference_server_message::Message::RunReady(ready),
                );
            }
            Some(Message::InvokeModel(invoke)) => {
                let id = invoke.invocation_id.clone();
                let Some(request) = invoke.request else {
                    invocation_error(&output, id, "RunInference model request is missing");
                    continue;
                };
                if invoke.delta_base.is_some() || invoke.compressed_request.is_some() {
                    invocation_error(
                        &output,
                        id,
                        "RunInference delta or compressed requests are not supported",
                    );
                    continue;
                }
                let request =
                    match project::project(&request, &model_id, run_context_window, &models) {
                        Ok(request) => request,
                        Err(error) => {
                            invocation_error(&output, id, &error.to_string());
                            continue;
                        }
                    };
                let context_window_tokens = request.model.context_window_tokens;
                let cancellation = cancellation.child_token();
                invocations.lock().insert(id.clone(), cancellation.clone());
                let recorder = start_recorder(
                    &calls,
                    &id,
                    &run_id,
                    &conversation_id,
                    provider_call_index,
                    &request,
                    model.as_ref(),
                )
                .await;
                tasks.spawn(serve_invocation(
                    plugins.clone(),
                    output.clone(),
                    invocations.clone(),
                    cancellation,
                    recorder,
                    ModelInvocation {
                        call_id: id,
                        run_id: run_id.clone(),
                        conversation_id: conversation_id.clone(),
                        provider_call_index,
                        request,
                    },
                    context_window_tokens,
                ));
                provider_call_index += 1;
            }
            Some(Message::CancelInvocation(cancel)) => {
                if let Some(token) = invocations.lock().remove(&cancel.invocation_id) {
                    token.cancel();
                }
            }
            Some(Message::FinishRun(finish)) => {
                finished = true;
                if finish.aborted {
                    for (_, token) in invocations.lock().drain() {
                        token.cancel();
                    }
                }
                break;
            }
            Some(Message::ResolveSubagentModels(resolve)) => {
                let resolved = resolve_subagent_models(resolve, &models);
                emit(
                    &output,
                    pb::run_inference_server_message::Message::SubagentModelsResolved(resolved),
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
    if !finished {
        for (_, token) in invocations.lock().drain() {
            token.cancel();
        }
    }
    while tasks.join_next().await.is_some() {}
    if !disconnected {
        output.emit(connect::encode_end_stream());
    }
    output.close();
}

fn run_ready(
    request: &pb::RunInferenceRunRequest,
    model: Option<&LocalModel>,
    models: &[PluginModelDescriptor],
    model_id: &str,
    context_window_tokens: Option<u64>,
    non_file_rules: Vec<agent::CursorRule>,
) -> pb::RunInferenceRunReady {
    let (display_name, vendor) = model.map_or_else(
        || (String::new(), "openai".into()),
        |model| (model.display_name.clone(), model.vendor.clone()),
    );
    let anthropic = vendor.eq_ignore_ascii_case("anthropic")
        || vendor.eq_ignore_ascii_case("messages")
        || display_name.to_ascii_lowercase().contains("claude");
    let name = display_name.to_ascii_lowercase();
    let claude = anthropic && name.contains("claude");
    let gpt5 = !anthropic && name.starts_with("gpt-5");
    pb::RunInferenceRunReady {
        resolved_model: request.requested_model.clone(),
        supports_self_summary: true,
        summarization: summarization_config(request, context_window_tokens),
        routed_model_display_name: (!display_name.is_empty()).then_some(display_name),
        prompt_model_metadata: Some(pb::RunInferencePromptModelMetadata {
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
            subagent_model_config: Some(subagent_model_config(model_id, models, request)),
            ..Default::default()
        }),
        non_file_rules,
        accepts_aborted_finish: true,
        accepts_subagent_model_resolution: true,
        ..Default::default()
    }
}

fn summarization_config(
    request: &pb::RunInferenceRunRequest,
    context_window_tokens: Option<u64>,
) -> Option<pb::RunInferenceSummarizationConfig> {
    Some(pb::RunInferenceSummarizationConfig {
        model: request.requested_model.clone(),
        is_fallback_to_main_model: true,
        max_prompt_chars: summary_prompt_chars(context_window_tokens),
        background: Some(pb::RunInferenceBackgroundSummarizationConfig {
            unused_percent_tokens_threshold_to_start: Some(SUMMARY_UNUSED_WINDOW_PERCENT),
            unused_percent_tokens_threshold_to_persist: Some(SUMMARY_UNUSED_WINDOW_PERCENT),
            ..Default::default()
        }),
    })
}

fn summary_prompt_chars(context_window_tokens: Option<u64>) -> i32 {
    match context_window_tokens {
        Some(window) if window > 0 && window < SUMMARY_PROMPT_SCALE_WINDOW => i32::try_from(
            (DEFAULT_SUMMARY_PROMPT_CHARS as u64).saturating_mul(window)
                / SUMMARY_PROMPT_SCALE_WINDOW,
        )
        .unwrap_or(DEFAULT_SUMMARY_PROMPT_CHARS),
        _ => DEFAULT_SUMMARY_PROMPT_CHARS,
    }
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
    let byok = models
        .iter()
        .map(|model| ByokModel {
            hash: model.id.clone(),
            model_id: model.model_id.clone(),
            display_name: model.display_name.clone(),
        })
        .collect::<Vec<_>>();
    let resolved = resolve
        .requested_models
        .iter()
        .map(|requested| {
            let hash = if byok.iter().any(|model| &model.hash == requested) {
                Some(requested.clone())
            } else {
                subagent_models::resolve(&byok, requested)
                    .ok()
                    .map(str::to_owned)
                    .filter(|hash| byok.iter().any(|model| &model.hash == hash))
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
    while let Some(next) = tokio::select! {
        () = cancellation.cancelled() => None,
        next = stream.next() => next,
    } {
        match next {
            Ok(event) => {
                let _ = recorder.event(&event).await;
                for response in model_responses(event, &mut tool_calls, context_window_tokens) {
                    emit(
                        &output,
                        pb::run_inference_server_message::Message::InvocationResponse(
                            pb::RunInferenceInvocationResponse {
                                invocation_id: id.clone(),
                                response: Some(response),
                            },
                        ),
                    );
                }
            }
            Err(value) => {
                let message = value.to_string();
                if let Some(response) = overflow_response(&message) {
                    emit(
                        &output,
                        pb::run_inference_server_message::Message::InvocationResponse(
                            pb::RunInferenceInvocationResponse {
                                invocation_id: id.clone(),
                                response: Some(response),
                            },
                        ),
                    );
                }
                let _ = recorder.failed(&value).await;
                error = Some(message);
                break;
            }
        }
    }
    if !recorder.is_finished() {
        if cancellation.is_cancelled() {
            let _ = recorder.cancelled().await;
        } else {
            let ended = Error::Provider("provider stream ended without Done".into());
            let _ = recorder.failed(&ended).await;
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

fn model_responses(
    event: ModelEvent,
    tool_calls: &mut HashMap<usize, (String, String, String)>,
    context_window_tokens: Option<u64>,
) -> Vec<pb::InferenceStreamResponse> {
    use pb::inference_stream_response::Response;
    let response = match event {
        ModelEvent::Usage(usage) => {
            return usage_responses(usage, context_window_tokens);
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
                return Vec::new();
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
                return Vec::new();
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
        | ModelEvent::Done(_) => return Vec::new(),
    };
    vec![pb::InferenceStreamResponse {
        response: Some(response),
        ..Default::default()
    }]
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
            prompt_tokens: count(prompt_tokens),
            completion_tokens: count(completion_tokens),
            total_tokens: total_tokens.map(count_value),
        })),
        stream_response(Response::ExtendedUsage(pb::InferenceExtendedUsageInfo {
            input_tokens: count(usage.input_tokens),
            output_tokens: count(usage.output_tokens),
            cache_read_tokens: count(usage.cache_read_tokens),
            cache_write_tokens: count(usage.cache_write_tokens),
            max_tokens: count(context_window_tokens),
            reasoning_tokens: usage.reasoning_tokens.map(count_value),
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

fn count(value: Option<u64>) -> i32 {
    count_value(value.unwrap_or_default())
}

fn count_value(value: u64) -> i32 {
    value.min(i32::MAX as u64) as i32
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

pub(crate) fn decode_append(data: &[u8]) -> Result<pb::RunInferenceClientMessage> {
    pb::RunInferenceClientMessage::decode(data).map_err(Error::from)
}

async fn start_recorder(
    store: &CallStore,
    call_id: &str,
    run_id: &str,
    conversation_id: &str,
    provider_call_index: u64,
    request: &crate::model::ModelRequest,
    model: Option<&LocalModel>,
) -> CallRecorder {
    let (stored_id, provider_type, request_url) = plugin_request_url(&request.model.model_id);
    let display_name = request
        .model
        .display_name
        .clone()
        .or_else(|| model.map(|model| model.display_name.clone()))
        .unwrap_or_else(|| stored_id.clone());
    CallRecorder::start(
        store.clone(),
        NewLlmCall {
            call_id: call_id.into(),
            run_id: run_id.into(),
            conversation_id: conversation_id.into(),
            provider_call_index: provider_call_index.min(i64::MAX as u64) as i64,
            model_hash: request.model.model_id.clone(),
            provider_type: provider_type.clone(),
            provider_url: request_url.clone(),
            request_type: provider_type,
            request_url,
            model_id: stored_id,
            display_name,
            reasoning_effort: request.model.reasoning.effort.clone(),
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
            output: Arc::new(OutputHub::default()),
            next_seqno: AsyncMutex::new(0),
            subscriptions: Mutex::default(),
            cancellation: CancellationToken::new(),
        });
        service
            .state
            .lock()
            .insert("run".into(), InferenceRoute::Local(run.clone()));
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
        assert!(snapshot.calls.is_empty());
        assert_eq!(snapshot.storage.call_count, 0);
    }

    #[test]
    fn output_replays_until_close_then_releases_its_history() {
        let output = OutputHub::default();
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
