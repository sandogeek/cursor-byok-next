//! Plugin protocol adapters: request bodies, HTTP/SSE, and normalized events.

mod anthropic;
mod chat;
mod responses;

use std::{collections::HashMap, time::Duration};

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{
    Error, Result,
    worker::{HostContext, InvocationState},
};
use crate::{
    model::{
        ContentPart, ModelInvocation, ProjectedContent, ProjectedMessage, ProviderReplayState,
        Role, ToolCallContent, ToolResultContent,
    },
    provider::ModelEvent,
};

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_STREAM_BYTES: u64 = 256 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolKind {
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProtocolStreamInput {
    protocol: String,
    url: String,
    model: String,
    #[serde(default)]
    headers: HashMap<String, String>,
}

impl ProtocolKind {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "openai-chat" => Ok(Self::OpenAiChat),
            "openai-responses" => Ok(Self::OpenAiResponses),
            "anthropic" => Ok(Self::Anthropic),
            other => Err(Error::Protocol(format!("unknown protocol: {other}"))),
        }
    }

    fn extra_headers(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Anthropic => &[("anthropic-version", "2023-06-01")],
            Self::OpenAiChat | Self::OpenAiResponses => &[],
        }
    }
}

pub(super) async fn stream(
    host: &HostContext,
    invocation: &InvocationState,
    params: Value,
) -> Result<Value> {
    let input: ProtocolStreamInput = serde_json::from_value(params).map_err(|error| {
        Error::Protocol(format!("invalid protocol.stream params: {error}"))
    })?;
    let protocol = ProtocolKind::parse(&input.protocol)?;
    let model = invocation.model()?;
    let tool_names = ToolNames::new(protocol, model);
    let body = match protocol {
        ProtocolKind::OpenAiChat => chat::build_body(&input.model, model, &tool_names)?,
        ProtocolKind::OpenAiResponses => responses::build_body(&input.model, model, &tool_names)?,
        ProtocolKind::Anthropic => anthropic::build_body(&input.model, model)?,
    };
    let body_text = serde_json::to_string(&body)?;
    let mut headers = HashMap::from([
        ("accept".into(), "text/event-stream".into()),
        ("content-type".into(), "application/json".into()),
    ]);
    for (name, value) in input.headers {
        headers.insert(name, value);
    }
    for (name, value) in protocol.extra_headers() {
        headers.insert((*name).into(), (*value).into());
    }
    let (builder, recorder) = host
        .prepare_request(
            invocation,
            &input.url,
            reqwest::Method::POST,
            &headers,
            Some(&body_text),
        )
        .await?;
    let cancellation = invocation.cancellation.clone();
    let mut response = send_cancellable(builder, &cancellation).await?;
    let status = response.status().as_u16();
    if let Some(recorder) = &recorder {
        recorder.response_headers(status).await?;
    }
    if !(200..300).contains(&status) {
        let body = read_error_body(
            &mut response,
            &cancellation,
            recorder.as_ref(),
            ERROR_BODY_TIMEOUT,
        )
        .await?;
        return Err(Error::HttpStatus { status, body });
    }
    let mut parser: Box<dyn ProtocolParser> = match protocol {
        ProtocolKind::OpenAiChat => Box::new(chat::Parser::new(tool_names)),
        ProtocolKind::OpenAiResponses => Box::new(responses::Parser::new(tool_names)),
        ProtocolKind::Anthropic => Box::new(anthropic::Parser::new()),
    };
    read_sse(
        response,
        &cancellation,
        recorder.as_ref(),
        parser.as_mut(),
        |event| invocation.emit(event),
    )
    .await?;
    parser.finish(&mut |event| invocation.emit(event))?;
    Ok(Value::Null)
}

trait ProtocolParser: Send {
    fn push_data(
        &mut self,
        payload: &str,
        event: &str,
        emit: &mut dyn FnMut(ModelEvent),
    ) -> Result<bool>;
    fn finish(&mut self, emit: &mut dyn FnMut(ModelEvent)) -> Result<()>;
}

async fn send_cancellable(
    request: reqwest::RequestBuilder,
    cancellation: &CancellationToken,
) -> Result<reqwest::Response> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(Error::Cancelled),
        () = tokio::time::sleep(REQUEST_TIMEOUT) => Err(Error::Provider(format!(
            "plugin network request timed out after {} seconds",
            REQUEST_TIMEOUT.as_secs()
        ))),
        response = request.send() => {
            response.map_err(|error| {
                Error::Provider(format!("plugin network request failed: {error}"))
            })
        }
    }
}

async fn read_error_body(
    response: &mut reqwest::Response,
    cancellation: &CancellationToken,
    recorder: Option<&crate::calls::CallRecorder>,
    timeout: Duration,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut body = Vec::new();
    while body.len() < MAX_ERROR_BODY_BYTES {
        let chunk = tokio::select! {
            _ = cancellation.cancelled() => return Err(Error::Cancelled),
            _ = tokio::time::sleep_until(deadline) => break,
            chunk = response.chunk() => match chunk {
                Ok(chunk) => chunk,
                Err(_) => break,
            },
        };
        let Some(chunk) = chunk else { break };
        if let Some(recorder) = recorder {
            let _ = recorder.response_chunk(&chunk).await;
        }
        let remaining = MAX_ERROR_BODY_BYTES - body.len();
        body.extend_from_slice(&chunk[..remaining.min(chunk.len())]);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

async fn read_sse(
    response: reqwest::Response,
    cancellation: &CancellationToken,
    recorder: Option<&crate::calls::CallRecorder>,
    parser: &mut dyn ProtocolParser,
    mut emit: impl FnMut(ModelEvent),
) -> Result<()> {
    let mut body = response.bytes_stream();
    let mut buffered = Vec::<u8>::new();
    let mut frame = SseFrame::default();
    let mut total = 0_u64;
    loop {
        let chunk = tokio::select! {
            _ = cancellation.cancelled() => return Err(Error::Cancelled),
            () = tokio::time::sleep(STREAM_IDLE_TIMEOUT) => {
                return Err(Error::Provider(format!(
                    "plugin network stream idle timeout: no events received for {} seconds",
                    STREAM_IDLE_TIMEOUT.as_secs()
                )));
            }
            chunk = body.next() => chunk,
        };
        let Some(chunk) = chunk else { break };
        let chunk = chunk.map_err(|error| {
            Error::Provider(format!("cannot read plugin network stream: {error}"))
        })?;
        total += chunk.len() as u64;
        if total > MAX_STREAM_BYTES {
            return Err(Error::Provider(
                "plugin network stream is larger than allowed".into(),
            ));
        }
        if let Some(recorder) = recorder {
            recorder.response_chunk(&chunk).await?;
        }
        buffered.extend_from_slice(&chunk);
        if push_buffered_lines(parser, &mut buffered, &mut frame, &mut emit)? {
            return Ok(());
        }
    }
    if !buffered.is_empty()
        && push_sse_line(
            parser,
            &mut frame,
            &String::from_utf8_lossy(&buffered),
            &mut emit,
        )?
    {
        return Ok(());
    }
    if let Some((event, payload)) = frame.take() {
        parser.push_data(&payload, &event, &mut emit)?;
    }
    Ok(())
}

fn push_buffered_lines(
    parser: &mut dyn ProtocolParser,
    buffered: &mut Vec<u8>,
    frame: &mut SseFrame,
    emit: &mut dyn FnMut(ModelEvent),
) -> Result<bool> {
    let mut consumed = 0;
    while let Some(relative) = buffered[consumed..].iter().position(|byte| *byte == b'\n') {
        let end = consumed + relative;
        let mut line = buffered[consumed..end].to_vec();
        consumed = end + 1;
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if push_sse_line(parser, frame, &String::from_utf8_lossy(&line), emit)? {
            buffered.drain(..consumed);
            return Ok(true);
        }
    }
    if consumed > 0 {
        buffered.copy_within(consumed.., 0);
        buffered.truncate(buffered.len() - consumed);
    }
    Ok(false)
}

fn push_sse_line(
    parser: &mut dyn ProtocolParser,
    frame: &mut SseFrame,
    line: &str,
    emit: &mut dyn FnMut(ModelEvent),
) -> Result<bool> {
    let Some((event, payload)) = frame.push_line(line) else {
        return Ok(false);
    };
    parser.push_data(&payload, &event, emit)
}

#[derive(Default)]
struct SseFrame {
    event: String,
    data: Vec<String>,
}

impl SseFrame {
    fn push_line(&mut self, line: &str) -> Option<(String, String)> {
        if line.is_empty() {
            return self.take();
        }
        if line.starts_with(':') {
            return None;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            self.event = sse_field(rest).to_owned();
            return None;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            self.data.push(sse_field(rest).to_owned());
        }
        None
    }

    fn take(&mut self) -> Option<(String, String)> {
        if self.data.is_empty() {
            self.event.clear();
            return None;
        }
        let payload = self.data.join("\n");
        self.data.clear();
        let event = std::mem::take(&mut self.event);
        if payload.trim().is_empty() {
            None
        } else {
            Some((event, payload))
        }
    }
}

fn sse_field(value: &str) -> &str {
    value.strip_prefix(' ').unwrap_or(value)
}

fn object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

fn text(value: &Value) -> Option<&str> {
    value.as_str()
}

fn count_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_f64()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|value| value as u64)
        }),
        _ => None,
    }
}

fn count_usize(value: &Value) -> Option<usize> {
    count_u64(value).map(|value| value as usize)
}

fn parse_object(payload: &str, label: &str) -> Result<Value> {
    match serde_json::from_str::<Value>(payload) {
        Ok(Value::Object(map)) => Ok(Value::Object(map)),
        Ok(_) => Ok(Value::Object(serde_json::Map::new())),
        Err(_) => Err(Error::Provider(format!("{label} SSE returned invalid JSON"))),
    }
}

fn encode_base64(data: &[u8]) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    STANDARD.encode(data)
}

fn data_url(mime_type: &str, data: &[u8]) -> String {
    format!("data:{mime_type};base64,{}", encode_base64(data))
}

fn joined_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            ContentPart::Image { .. } | ContentPart::File { .. } => None,
        })
        .collect()
}

enum CoreMessage<'a> {
    System(&'a [ContentPart]),
    User(&'a [ContentPart]),
    Assistant {
        text: std::borrow::Cow<'a, str>,
        replay_state: Option<&'a ProviderReplayState>,
        tool_calls: &'a [ToolCallContent],
    },
    Tool(&'a ToolResultContent),
}

fn core_message(message: &ProjectedMessage) -> Result<CoreMessage<'_>> {
    match &message.content {
        ProjectedContent::Parts(parts) => match message.role {
            Role::System => Ok(CoreMessage::System(parts)),
            Role::User => Ok(CoreMessage::User(parts)),
            Role::Assistant => Ok(CoreMessage::Assistant {
                text: std::borrow::Cow::Owned(joined_text(parts)),
                replay_state: None,
                tool_calls: &[],
            }),
            Role::Tool => Err(Error::Protocol(
                "tool messages must carry a tool result".into(),
            )),
        },
        ProjectedContent::Assistant {
            text,
            replay_state,
            calls,
            ..
        } => Ok(CoreMessage::Assistant {
            text: std::borrow::Cow::Borrowed(text),
            replay_state: replay_state.as_ref(),
            tool_calls: calls,
        }),
        ProjectedContent::ToolResult(result) => Ok(CoreMessage::Tool(result)),
    }
}

fn usage_event(
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
) -> ModelEvent {
    ModelEvent::Usage(crate::model::Usage {
        input_tokens,
        context_input_tokens: input_tokens,
        output_tokens,
        total_tokens,
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens,
    })
}

fn truncated_call_id(call_id: &str) -> &str {
    match call_id.char_indices().nth(64) {
        Some((end, _)) => &call_id[..end],
        None => call_id,
    }
}

fn openai_tool_name(name: &str) -> String {
    let normalized = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if normalized.is_empty() {
        "_".into()
    } else {
        normalized
    }
}

/// One-to-one mapping between Cursor's tool names and the names the OpenAI
/// protocols accept. The provider only ever sees the alias; events sent back to
/// Cursor carry the original name.
#[derive(Clone, Debug, Default)]
pub(super) struct ToolNames {
    by_name: HashMap<String, String>,
    by_alias: HashMap<String, String>,
}

impl ToolNames {
    fn new(protocol: ProtocolKind, invocation: &ModelInvocation) -> Self {
        let mut names = Self::default();
        if protocol == ProtocolKind::Anthropic {
            return names;
        }
        for tool in &invocation.request.prompt.tools {
            names.insert(&tool.name);
        }
        for message in &invocation.request.history {
            if let ProjectedContent::Assistant { calls, .. } = &message.content {
                for call in calls {
                    names.insert(&call.name);
                }
            }
        }
        names
    }

    fn insert(&mut self, name: &str) {
        if self.by_name.contains_key(name) {
            return;
        }
        let base = openai_tool_name(name);
        let mut alias = base.clone();
        let mut suffix = 1;
        while self.by_alias.contains_key(&alias) {
            suffix += 1;
            alias = format!("{base}_{suffix}");
        }
        self.by_alias.insert(alias.clone(), name.to_owned());
        self.by_name.insert(name.to_owned(), alias);
    }

    fn alias(&self, name: &str) -> String {
        self.by_name
            .get(name)
            .cloned()
            .unwrap_or_else(|| openai_tool_name(name))
    }

    fn restore(&self, alias: &str) -> String {
        self.by_alias
            .get(alias)
            .cloned()
            .unwrap_or_else(|| alias.to_owned())
    }
}

fn apply_stop_sequences(body: &mut Value, key: &str, sequences: &[String]) -> Result<()> {
    if sequences.is_empty() {
        return Ok(());
    }
    let body = body
        .as_object_mut()
        .ok_or_else(|| Error::Provider("provider request body must be an object".into()))?;
    body.insert(key.to_owned(), Value::from(sequences));
    Ok(())
}

fn merge_extra_params(body: &mut Value, extra: &Value) -> Result<()> {
    let Some(extra) = extra.as_object() else {
        if extra.is_null() {
            return Ok(());
        }
        return Err(Error::Protocol(
            "model extra params must be an object".into(),
        ));
    };
    if extra.is_empty() {
        return Ok(());
    }
    let body = body.as_object_mut().ok_or_else(|| {
        Error::Provider("provider request body must be an object".into())
    })?;
    for (name, value) in extra {
        if matches!(
            name.as_str(),
            "model"
                | "stream"
                | "messages"
                | "input"
                | "tools"
                | "system"
                | "instructions"
                | "prompt_cache_key"
                | "stop"
                | "stop_sequences"
        ) {
            return Err(Error::Protocol(format!(
                "model extra params cannot replace {name}"
            )));
        }
        body.insert(name.clone(), value.clone());
    }
    Ok(())
}

fn apply_openai_prompt_cache_key(body: &mut Value, model_id: &str, conversation_id: &str) {
    if !model_id.to_ascii_lowercase().contains("gpt") {
        return;
    }
    if let Some(object) = body.as_object_mut() {
        object.insert(
            "prompt_cache_key".into(),
            conversation_id.to_owned().into(),
        );
    }
}

#[cfg(test)]
fn collect_events(
    parser: &mut dyn ProtocolParser,
    payloads: &[&str],
) -> Result<Vec<ModelEvent>> {
    let mut events = Vec::new();
    for payload in payloads {
        if parser.push_data(payload, "", &mut |event| events.push(event))? {
            break;
        }
    }
    parser.finish(&mut |event| events.push(event))?;
    Ok(events)
}

#[cfg(test)]
mod tests {
    use crate::model::{
        ContentPart, ModelInvocation, ModelRequest, ModelSpec, ProjectedContent, ProjectedMessage,
        PromptSpec, ProviderReplayState, Role, ToolCallContent, ToolDefinition, ToolResultContent,
    };
    use crate::plugins::adapters::{ProtocolKind, ToolNames, collect_events};
    use crate::provider::ModelEvent;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn names() -> ToolNames {
        ToolNames::default()
    }

    pub(crate) fn invocation() -> ModelInvocation {
        ModelInvocation {
            call_id: "call-1".into(),
            run_id: "run-1".into(),
            conversation_id: "conversation-1".into(),
            provider_call_index: 0,
            request: ModelRequest {
                prompt: PromptSpec {
                    instructions: "You are concise.".into(),
                    tools: Vec::new(),
                },
                model: ModelSpec::new("plugin:p/c/m"),
                history: vec![ProjectedMessage {
                    message_id: "m1".into(),
                    role: Role::User,
                    content: ProjectedContent::Parts(vec![ContentPart::Text {
                        text: "hello".into(),
                    }]),
                }],
            },
        }
    }

    fn tool_roundtrip() -> ModelInvocation {
        let mut invocation = invocation();
        invocation.request.prompt.tools = vec![ToolDefinition {
            name: "lookup".into(),
            description: "Look up".into(),
            parameters: serde_json::json!({ "type": "object" }),
        }];
        invocation.request.history = vec![
            ProjectedMessage {
                message_id: "m1".into(),
                role: Role::User,
                content: ProjectedContent::Parts(vec![ContentPart::Text { text: "ask".into() }]),
            },
            ProjectedMessage {
                message_id: "m2".into(),
                role: Role::Assistant,
                content: ProjectedContent::Assistant {
                    text: String::new(),
                    thinking: String::new(),
                    replay_state: Some(ProviderReplayState {
                        provider_kind: "openai_chat".into(),
                        value: serde_json::json!({ "reasoning_content": "why" }),
                    }),
                    calls: vec![ToolCallContent {
                        index: 0,
                        call_id: "call-1".into(),
                        name: "lookup".into(),
                        arguments: serde_json::json!({ "q": "a" }),
                    }],
                },
            },
            ProjectedMessage {
                message_id: "m3".into(),
                role: Role::Tool,
                content: ProjectedContent::ToolResult(ToolResultContent {
                    call_id: "call-1".into(),
                    name: "lookup".into(),
                    content: "data".into(),
                    is_error: false,
                    image: None,
                    provider_parts: Vec::new(),
                }),
            },
            ProjectedMessage {
                message_id: "m4".into(),
                role: Role::User,
                content: ProjectedContent::Parts(vec![
                    ContentPart::Text { text: "see".into() },
                    ContentPart::Image {
                        mime_type: "image/png".into(),
                        data: vec![0, 1, 2, 3],
                    },
                    ContentPart::File {
                        mime_type: "application/pdf".into(),
                        filename: Some("report.pdf".into()),
                        data: b"%PDF-1".to_vec(),
                    },
                ]),
            },
        ];
        invocation
    }

    fn illegal_tool() -> ModelInvocation {
        let mut invocation = invocation();
        invocation.request.history = vec![ProjectedMessage {
            message_id: "m1".into(),
            role: Role::Tool,
            content: ProjectedContent::Parts(vec![ContentPart::Text { text: "oops".into() }]),
        }];
        invocation
    }

    #[test]
    fn omits_empty_optional_fields() {
        let body = super::chat::build_body("gpt-4.1", &invocation(), &names()).unwrap();
        assert!(body.get("max_completion_tokens").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("service_tier").is_none());
        assert!(body.get("tools").is_none());
        assert!(body.get("stop").is_none());
        let body = super::responses::build_body("gpt-5", &invocation(), &names()).unwrap();
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("reasoning").is_none());
        assert!(body.get("service_tier").is_none());
        assert!(body.get("tools").is_none());
        assert!(body.get("stop").is_none());
        let body = super::anthropic::build_body("claude", &invocation()).unwrap();
        assert!(body.get("tools").is_none());
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
        assert!(body.get("stop_sequences").is_none());
    }

    #[test]
    fn maps_stop_sequences_per_protocol() {
        let mut invocation = invocation();
        invocation.request.model.stop_sequences = vec!["STOP".into(), "\n\nUser:".into()];
        let chat = super::chat::build_body("gpt-4.1", &invocation, &names()).unwrap();
        assert_eq!(chat["stop"], serde_json::json!(["STOP", "\n\nUser:"]));
        let responses = super::responses::build_body("gpt-5", &invocation, &names()).unwrap();
        assert!(responses.get("stop").is_none());
        assert!(responses.get("stop_sequences").is_none());
        let anthropic = super::anthropic::build_body("claude", &invocation).unwrap();
        assert_eq!(
            anthropic["stop_sequences"],
            serde_json::json!(["STOP", "\n\nUser:"])
        );
        assert!(anthropic.get("stop").is_none());
    }

    #[test]
    fn restores_original_tool_names_after_openai_aliasing() {
        let mut invocation = invocation();
        invocation.request.prompt.tools = vec![
            ToolDefinition {
                name: "a.b".into(),
                description: String::new(),
                parameters: serde_json::json!({ "type": "object" }),
            },
            ToolDefinition {
                name: "a_b".into(),
                description: String::new(),
                parameters: serde_json::json!({ "type": "object" }),
            },
        ];
        let names = ToolNames::new(ProtocolKind::OpenAiChat, &invocation);
        assert_eq!(names.alias("a.b"), "a_b");
        assert_eq!(names.alias("a_b"), "a_b_2");
        assert_eq!(names.restore("a_b"), "a.b");
        assert_eq!(names.restore("a_b_2"), "a_b");
        let chat = super::chat::build_body("gpt-4.1", &invocation, &names).unwrap();
        assert_eq!(chat["tools"][0]["function"]["name"], "a_b");
        assert_eq!(chat["tools"][1]["function"]["name"], "a_b_2");
        let events = collect_events(
            &mut super::chat::Parser::new(names.clone()),
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"a_b_2","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
                "[DONE]",
            ],
        )
        .unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallStart { name, .. } if name == "a_b"
        )));

        let responses = super::responses::build_body("gpt-5", &invocation, &names).unwrap();
        assert_eq!(responses["tools"][0]["name"], "a_b");
        assert_eq!(responses["tools"][1]["name"], "a_b_2");
        let events = collect_events(
            &mut super::responses::Parser::new(names),
            &[
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call-1","name":"a_b"}}"#,
                r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{}"}"#,
                r#"{"type":"response.completed","response":{}}"#,
            ],
        )
        .unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallStart { name, .. } if name == "a.b"
        )));
    }

    #[test]
    fn keeps_anthropic_tool_names_unaliased() {
        let mut invocation = invocation();
        invocation.request.prompt.tools = vec![ToolDefinition {
            name: "a.b".into(),
            description: String::new(),
            parameters: serde_json::json!({ "type": "object" }),
        }];
        let names = ToolNames::new(ProtocolKind::Anthropic, &invocation);
        assert_eq!(names.restore("a.b"), "a.b");
        assert_eq!(
            super::anthropic::build_body("claude", &invocation).unwrap()["tools"][0]["name"],
            "a.b"
        );
    }

    #[test]
    fn rejects_tool_messages_without_a_result_before_any_protocol_body() {
        let invocation = illegal_tool();
        let chat = super::chat::build_body("gpt-4.1", &invocation, &names()).unwrap_err();
        let responses = super::responses::build_body("gpt-5", &invocation, &names()).unwrap_err();
        let anthropic = super::anthropic::build_body("claude", &invocation).unwrap_err();
        assert_eq!(chat.to_string(), "tool messages must carry a tool result");
        assert_eq!(responses.to_string(), chat.to_string());
        assert_eq!(anthropic.to_string(), chat.to_string());
    }

    #[test]
    fn builds_equivalent_bodies_from_the_same_core_fixture() {
        let invocation = tool_roundtrip();
        let names = ToolNames::new(ProtocolKind::OpenAiChat, &invocation);
        let chat = super::chat::build_body("gpt-4.1", &invocation, &names).unwrap();
        assert_eq!(chat["messages"][0]["role"], "system");
        assert_eq!(chat["messages"][3]["role"], "tool");
        assert_eq!(chat["messages"][3]["tool_call_id"], "call-1");
        assert_eq!(chat["messages"][4]["content"][1]["type"], "image_url");
        let responses = super::responses::build_body("gpt-5", &invocation, &names).unwrap();
        assert_eq!(responses["input"][1]["type"], "function_call");
        assert_eq!(responses["input"][2]["type"], "function_call_output");
        assert_eq!(responses["input"][3]["content"][1]["type"], "input_image");
        assert_eq!(responses["input"][3]["content"][2]["type"], "input_file");
        let anthropic = super::anthropic::build_body("claude", &invocation).unwrap();
        assert_eq!(anthropic["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(anthropic["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(anthropic["messages"][2]["content"][2]["type"], "image");
        assert_eq!(anthropic["messages"][2]["content"][3]["type"], "document");
    }

    #[test]
    fn merges_sampling_params_and_rejects_core_field_overrides() {
        let mut invocation = invocation();
        invocation.request.model.extra_params = serde_json::json!({
            "temperature": 0.2,
            "top_p": 0.9,
        });
        let chat = super::chat::build_body("gpt-4.1", &invocation, &names()).unwrap();
        assert_eq!(chat["temperature"], 0.2);
        assert_eq!(chat["top_p"], 0.9);
        invocation.request.model.extra_params = serde_json::json!({ "messages": [] });
        let error = super::chat::build_body("gpt-4.1", &invocation, &names()).unwrap_err();
        assert_eq!(error.to_string(), "model extra params cannot replace messages");
        invocation.request.model.extra_params = serde_json::json!({ "stop": [] });
        let error = super::chat::build_body("gpt-4.1", &invocation, &names()).unwrap_err();
        assert_eq!(error.to_string(), "model extra params cannot replace stop");
    }

    #[test]
    fn sse_frames_join_multiline_data_and_named_events() {
        let mut frame = super::SseFrame::default();
        assert!(frame.push_line("event: message").is_none());
        assert!(frame.push_line("data: {\"type\":").is_none());
        assert!(frame.push_line("data: \"error\"}").is_none());
        let (event, payload) = frame.push_line("").unwrap();
        assert_eq!(event, "message");
        assert_eq!(payload, "{\"type\":\n\"error\"}");
        assert!(frame.push_line(": keep-alive").is_none());
        assert!(frame.push_line("").is_none());
    }

    async fn stalled_error_response(
        body: &'static [u8],
        content_length: usize,
        stall: bool,
    ) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let header = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: {content_length}\r\nConnection: keep-alive\r\n\r\n"
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
            if stall {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .get(&origin)
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn error_body_read_keeps_the_status_and_never_waits_for_eof() {
        // The gateway announced more bytes than it sent and keeps the body open:
        // without a deadline this would hang until the run timed out.
        let mut stalled = stalled_error_response(b"unauthorized", 4096, true).await;
        let started = tokio::time::Instant::now();
        let body = super::read_error_body(
            &mut stalled,
            &CancellationToken::new(),
            None,
            Duration::from_millis(100),
        )
        .await
        .unwrap();
        assert_eq!(body, "unauthorized");
        assert!(started.elapsed() < Duration::from_secs(5));

        let oversized =
            Box::leak(vec![b'x'; super::MAX_ERROR_BODY_BYTES + 1024].into_boxed_slice());
        let mut capped = stalled_error_response(oversized, oversized.len(), false).await;
        let body = super::read_error_body(
            &mut capped,
            &CancellationToken::new(),
            None,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(body.len(), super::MAX_ERROR_BODY_BYTES);
    }
}
