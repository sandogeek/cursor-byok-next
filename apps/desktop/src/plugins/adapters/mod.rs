//! Plugin protocol adapters: request bodies, HTTP/SSE, and normalized events.

mod anthropic;
mod chat;
mod responses;
mod tools;

use std::{borrow::Cow, collections::HashMap, time::Duration};

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::{Error, Result, host_calls::HostContext, worker::InvocationState};
use crate::{
    model::{
        ContentPart, ModelInvocation, ProjectedContent, ProjectedMessage, ProviderReplayState,
        Role, ToolCallContent, ToolResultContent, joined_text,
    },
    provider::ModelEvent,
};

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_STREAM_BYTES: usize = 256 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_hours(1);
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_mins(30);
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
    let input: ProtocolStreamInput = serde_json::from_value(params)
        .map_err(|error| Error::Protocol(format!("invalid protocol.stream params: {error}")))?;
    let protocol = ProtocolKind::parse(&input.protocol)?;
    let model = invocation.model()?;
    let tool_names = ToolNames::new(model);
    let body = match protocol {
        ProtocolKind::OpenAiChat => chat::build_body(&input.model, model, &tool_names)?,
        ProtocolKind::OpenAiResponses => responses::build_body(&input.model, model, &tool_names)?,
        ProtocolKind::Anthropic => anthropic::build_body(&input.model, model, &tool_names)?,
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
        recorder.response_headers(status).await;
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
        ProtocolKind::Anthropic => Box::new(anthropic::Parser::new(tool_names)),
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
        () = cancellation.cancelled() => Err(Error::Cancelled),
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
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            () = tokio::time::sleep_until(deadline) => break,
            chunk = response.chunk() => match chunk {
                Ok(chunk) => chunk,
                Err(_) => break,
            },
        };
        let Some(chunk) = chunk else { break };
        if let Some(recorder) = recorder {
            recorder.response_chunk(&chunk).await;
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
    let mut total = 0_usize;
    loop {
        let chunk = tokio::select! {
            () = cancellation.cancelled() => return Err(Error::Cancelled),
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
        total = total.saturating_add(chunk.len());
        if total > MAX_STREAM_BYTES {
            return Err(Error::Provider(
                "plugin network stream is larger than allowed".into(),
            ));
        }
        if let Some(recorder) = recorder {
            recorder.response_chunk(&chunk).await;
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
            sse_field(rest).clone_into(&mut self.event);
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

/// Provider request bodies are JSON objects; every adapter builds one, so a
/// miss is a bug in the adapter rather than bad external input.
fn object_mut(value: &mut Value) -> Result<&mut serde_json::Map<String, Value>> {
    value
        .as_object_mut()
        .ok_or_else(|| Error::Provider("provider request body must be an object".into()))
}

/// Whether a stream reported at least one tool call. A `bool` here would read
/// as `map_finish(reason, true)` at the call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SawTool {
    Absent,
    Present,
}

impl SawTool {
    pub(super) fn from_count(count: usize) -> Self {
        if count == 0 {
            Self::Absent
        } else {
            Self::Present
        }
    }
}

/// The string under `key`, or `None` when the key is absent or not a string.
/// Provider payloads put optional fields on objects that may not exist, so
/// this replaces the `value.get(key).unwrap_or(&Value::Null).as_str()` spelling.
fn text_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// The string under `key` of an already-extracted object.
fn map_text_field<'a>(
    value: Option<&'a serde_json::Map<String, Value>>,
    key: &str,
) -> Option<&'a str> {
    value?.get(key)?.as_str()
}

fn count_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_f64()
                .and_then(crate::protocol::json::integral_u64)
        }),
        _ => None,
    }
}

fn count_usize(value: &Value) -> Option<usize> {
    count_u64(value).and_then(|value| usize::try_from(value).ok())
}

/// The integer under `key` of an already-extracted object.
fn map_count(value: Option<&serde_json::Map<String, Value>>, key: &str) -> Option<u64> {
    value?.get(key).and_then(count_u64)
}

/// The integer index under `key`, or `None` when it is absent, not a count, or
/// does not fit `usize`. Callers attach their own protocol-specific message.
pub(super) fn count_field(value: &Value, key: &str) -> Option<usize> {
    value.get(key).and_then(count_usize)
}

/// The provider's error detail for an SSE event, if the event is an error.
///
/// Every provider family puts the message and an optional error kind in
/// different places, so this reads all of the known spellings and leaves the
/// formatting to the adapter that knows its protocol.
fn event_error_detail(value: &Value) -> Option<(Option<String>, String)> {
    let is_error = text_field(value, "type") == Some("error")
        || value.get("error").is_some_and(|e| !e.is_null());
    if !is_error {
        return None;
    }
    let error = value.get("error").unwrap_or(&Value::Null);
    if let Some(text) = error.as_str() {
        return Some((None, text.to_owned()));
    }
    let details = error.as_object();
    let kind = details
        .and_then(|details| details.get("type"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let message = details
        .and_then(|details| details.get("message"))
        .and_then(Value::as_str)
        .or_else(|| text_field(value, "message"))
        .map(str::to_owned);
    match (kind, message) {
        (None, None) if error.is_null() => None,
        (kind, message) => Some((kind, message.unwrap_or_else(|| error.to_string()))),
    }
}

/// Parses one SSE payload. A JSON array or scalar is valid JSON but not an
/// event object, so it is reported as such instead of silently reading as `{}`.
fn parse_object(payload: &str, label: &str) -> Result<Value> {
    match serde_json::from_str::<Value>(payload) {
        Ok(Value::Object(map)) => Ok(Value::Object(map)),
        Ok(other) => Err(Error::Provider(format!(
            "{label} SSE returned {other} where an event object was expected"
        ))),
        Err(error) => Err(Error::Provider(format!(
            "{label} SSE returned invalid JSON: {error}"
        ))),
    }
}

fn encode_base64(data: &[u8]) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    STANDARD.encode(data)
}

fn data_url(mime_type: &str, data: &[u8]) -> String {
    format!("data:{mime_type};base64,{}", encode_base64(data))
}

enum CoreMessage<'a> {
    System(&'a [ContentPart]),
    User(&'a [ContentPart]),
    Assistant {
        text: Cow<'a, str>,
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
                text: Cow::Owned(joined_text(parts)),
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
            text: Cow::Borrowed(text),
            replay_state: replay_state.as_ref(),
            tool_calls: calls,
        }),
        ProjectedContent::ToolResult(result) => Ok(CoreMessage::Tool(result)),
    }
}

fn truncated_call_id(call_id: &str) -> &str {
    match call_id.char_indices().nth(64) {
        Some((end, _)) => &call_id[..end],
        None => call_id,
    }
}

/// `OpenAI` accepts `^[a-zA-Z0-9_-]{1,64}$`; Anthropic accepts a longer but
/// equally character-restricted pattern. 64 satisfies both.
const MAX_TOOL_NAME_CHARS: usize = 64;
const TOOL_NAME_HASH_CHARS: usize = 7;

/// Cursor names MCP tools `${serverIdentifier}-${toolName}` and passes provider
/// names through untouched, so `.`, `:` and spaces reach this layer verbatim.
fn provider_tool_name(name: &str) -> String {
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
    let base = if normalized.is_empty() {
        "_".to_owned()
    } else {
        normalized
    };
    truncated_tool_name(&base)
}

/// Long names keep their first 57 characters plus a content hash, so two names
/// that differ only past the cut still map to distinct aliases.
fn truncated_tool_name(name: &str) -> String {
    if name.chars().count() <= MAX_TOOL_NAME_CHARS {
        return name.to_owned();
    }
    let digest = hex::encode(Sha256::digest(name.as_bytes()));
    let prefix: String = name
        .chars()
        .take(MAX_TOOL_NAME_CHARS - TOOL_NAME_HASH_CHARS)
        .collect();
    format!("{prefix}{}", &digest[..TOOL_NAME_HASH_CHARS])
}

/// One-to-one mapping between Cursor's tool names and the names providers
/// accept. The provider only ever sees the alias; events sent back to Cursor
/// carry the original name.
#[derive(Clone, Debug, Default)]
pub(super) struct ToolNames {
    by_name: HashMap<String, String>,
    by_alias: HashMap<String, String>,
}

impl ToolNames {
    fn new(invocation: &ModelInvocation) -> Self {
        let mut names = Self::default();
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
        let base = provider_tool_name(name);
        let mut alias = base.clone();
        let mut suffix = 1;
        while self.by_alias.contains_key(&alias) {
            suffix += 1;
            alias = truncated_tool_name(&format!("{base}_{suffix}"));
        }
        self.by_alias.insert(alias.clone(), name.to_owned());
        self.by_name.insert(name.to_owned(), alias);
    }

    /// The provider-facing name. Registered tools borrow; only a name that was
    /// never inserted needs the sanitized fallback allocation.
    fn alias<'a>(&'a self, name: &'a str) -> Cow<'a, str> {
        self.by_name.get(name).map_or_else(
            || Cow::Owned(provider_tool_name(name)),
            |alias| Cow::Borrowed(alias.as_str()),
        )
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
    let body = body
        .as_object_mut()
        .ok_or_else(|| Error::Provider("provider request body must be an object".into()))?;
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
        object.insert("prompt_cache_key".into(), conversation_id.to_owned().into());
    }
}

#[cfg(test)]
fn collect_events(parser: &mut dyn ProtocolParser, payloads: &[&str]) -> Result<Vec<ModelEvent>> {
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
    use crate::plugins::adapters::{ToolNames, collect_events};
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
            content: ProjectedContent::Parts(vec![ContentPart::Text {
                text: "oops".into(),
            }]),
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
        let body =
            super::anthropic::build_body("claude", &invocation(), &ToolNames::new(&invocation()))
                .unwrap();
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
        let anthropic =
            super::anthropic::build_body("claude", &invocation, &ToolNames::new(&invocation))
                .unwrap();
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
        let names = ToolNames::new(&invocation);
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
    fn aliases_anthropic_tool_names_like_the_openai_protocols() {
        let mut invocation = invocation();
        invocation.request.prompt.tools = vec![ToolDefinition {
            name: "a.b".into(),
            description: String::new(),
            parameters: serde_json::json!({ "type": "object" }),
        }];
        let names = ToolNames::new(&invocation);
        assert_eq!(names.alias("a.b"), "a_b");
        assert_eq!(names.restore("a_b"), "a.b");
        let body = super::anthropic::build_body("claude", &invocation, &names).unwrap();
        assert_eq!(body["tools"][0]["name"], "a_b");

        let events = collect_events(
            &mut super::anthropic::Parser::new(names),
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call-1","name":"a_b"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        )
        .unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallStart { name, .. } if name == "a.b"
        )));
    }

    #[test]
    fn truncates_long_tool_names_to_the_provider_limit() {
        let long_server = "user-eamodio.gitlens-extension-GitKraken-pull_request_create_review";
        let sibling = "user-eamodio.gitlens-extension-GitKraken-pull_request_create_views";
        let mut invocation = invocation();
        invocation.request.prompt.tools = vec![
            ToolDefinition {
                name: long_server.into(),
                description: String::new(),
                parameters: serde_json::json!({ "type": "object" }),
            },
            ToolDefinition {
                name: sibling.into(),
                description: String::new(),
                parameters: serde_json::json!({ "type": "object" }),
            },
        ];
        let names = ToolNames::new(&invocation);
        let first = names.alias(long_server);
        let second = names.alias(sibling);
        assert_eq!(first.chars().count(), super::MAX_TOOL_NAME_CHARS);
        assert_eq!(second.chars().count(), super::MAX_TOOL_NAME_CHARS);
        assert_ne!(first, second);
        assert_eq!(names.restore(&first), long_server);
        assert_eq!(names.restore(&second), sibling);
        for alias in [&first, &second] {
            assert!(alias.chars().all(
                |character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            ));
        }
    }

    #[test]
    fn rejects_tool_messages_without_a_result_before_any_protocol_body() {
        let invocation = illegal_tool();
        let chat = super::chat::build_body("gpt-4.1", &invocation, &names()).unwrap_err();
        let responses = super::responses::build_body("gpt-5", &invocation, &names()).unwrap_err();
        let anthropic =
            super::anthropic::build_body("claude", &invocation, &ToolNames::new(&invocation))
                .unwrap_err();
        assert_eq!(chat.to_string(), "tool messages must carry a tool result");
        assert_eq!(responses.to_string(), chat.to_string());
        assert_eq!(anthropic.to_string(), chat.to_string());
    }

    #[test]
    fn builds_equivalent_bodies_from_the_same_core_fixture() {
        let invocation = tool_roundtrip();
        let names = ToolNames::new(&invocation);
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
        let anthropic =
            super::anthropic::build_body("claude", &invocation, &ToolNames::new(&invocation))
                .unwrap();
        assert_eq!(anthropic["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(
            anthropic["messages"][2]["content"][0]["type"],
            "tool_result"
        );
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
        assert_eq!(
            error.to_string(),
            "model extra params cannot replace messages"
        );
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

    /// Whether the test origin leaves the response body open after the payload.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum StalledBody {
        Closes,
        Hangs,
    }

    async fn stalled_error_response(
        body: &'static [u8],
        content_length: usize,
        behavior: StalledBody,
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
            if behavior == StalledBody::Hangs {
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
        let mut stalled = stalled_error_response(b"unauthorized", 4096, StalledBody::Hangs).await;
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
        let mut capped =
            stalled_error_response(oversized, oversized.len(), StalledBody::Closes).await;
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
