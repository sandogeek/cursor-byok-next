//! Anthropic Messages request body and SSE → `ModelEvent`.

use serde_json::{Value, json};

use super::{
    CoreMessage, ProtocolParser, SawTool, ToolNames, core_message, count_usize, encode_base64,
    object, object_mut, parse_object, text,
};
use crate::{
    model::{ContentPart, ModelInvocation},
    plugins::{Error, Result},
    provider::{FinishReason, ModelEvent},
};

const REPLAY_KIND: &str = "anthropic";
const DEFAULT_MAX_TOKENS: u64 = 65_000;

/// Anthropic rejects images and documents outside user turns, where `OpenAI`
/// accepts them anywhere. A `bool` here would read as `anthropic_parts(.., true)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attachments {
    Allowed,
    Forbidden,
}

struct UsageState {
    input: Option<u64>,
    output: Option<u64>,
    total: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    reasoning: Option<u64>,
}

pub(super) struct Parser {
    tool_names: ToolNames,
    block_types: std::collections::BTreeMap<usize, String>,
    thinking_text: std::collections::BTreeMap<usize, String>,
    thinking_signatures: std::collections::BTreeMap<usize, String>,
    thinking_blocks: Vec<Value>,
    usage: UsageState,
    saw_usage: bool,
    saw_tool: SawTool,
    finish: Option<FinishReason>,
    terminal: bool,
}

impl Parser {
    pub(super) fn new(tool_names: ToolNames) -> Self {
        Self {
            tool_names,
            block_types: std::collections::BTreeMap::new(),
            thinking_text: std::collections::BTreeMap::new(),
            thinking_signatures: std::collections::BTreeMap::new(),
            thinking_blocks: Vec::new(),
            usage: empty_usage(),
            saw_usage: false,
            saw_tool: SawTool::Absent,
            finish: None,
            terminal: false,
        }
    }

    fn close_block(&mut self, index: usize, kind: &str, emit: &mut dyn FnMut(ModelEvent)) {
        match kind {
            "text" => emit(ModelEvent::TextEnd),
            "thinking" => {
                let thinking = self.thinking_text.remove(&index).unwrap_or_default();
                let signature = self.thinking_signatures.remove(&index).unwrap_or_default();
                emit(ModelEvent::ThinkingEnd);
                if !signature.is_empty() {
                    self.thinking_blocks.push(json!({
                        "type": "thinking",
                        "thinking": thinking,
                        "signature": signature,
                    }));
                    emit(ModelEvent::ThinkingSignature(signature));
                }
            }
            "tool_use" => emit(ModelEvent::ToolCallEnd { index }),
            _ => {}
        }
    }

    fn close_remaining(&mut self, emit: &mut dyn FnMut(ModelEvent)) {
        let remaining: Vec<_> = self
            .block_types
            .iter()
            .map(|(index, kind)| (*index, kind.clone()))
            .collect();
        self.block_types.clear();
        for (index, kind) in remaining {
            self.close_block(index, &kind, emit);
        }
    }

    fn message_start(&mut self, value: &Value) {
        if let Some(message_usage) = value
            .get("message")
            .and_then(object)
            .and_then(|message| message.get("usage"))
        {
            self.saw_usage = true;
            merge_usage(&mut self.usage, &anthropic_usage(message_usage));
        }
    }

    fn content_block_start(
        &mut self,
        value: &Value,
        emit: &mut dyn FnMut(ModelEvent),
    ) -> Result<()> {
        let index = required_index(value)?;
        let block = value
            .get("content_block")
            .and_then(object)
            .cloned()
            .unwrap_or_default();
        let kind = required_type(&block)?;
        self.block_types.insert(index, kind.clone());
        match kind.as_str() {
            "text" => emit(ModelEvent::TextStart),
            "thinking" => {
                self.thinking_text.insert(index, String::new());
                self.thinking_signatures.insert(index, String::new());
                emit(ModelEvent::ThinkingStart);
            }
            "redacted_thinking" => {
                let data = text(block.get("data").unwrap_or(&Value::Null))
                    .ok_or_else(|| Error::Provider("Anthropic event is missing data".into()))?;
                self.thinking_blocks
                    .push(json!({ "type": "redacted_thinking", "data": data }));
                emit(ModelEvent::RedactedThinking(data.to_owned()));
            }
            "tool_use" => {
                let call_id = text(block.get("id").unwrap_or(&Value::Null));
                let name = text(block.get("name").unwrap_or(&Value::Null));
                let (Some(call_id), Some(name)) = (call_id, name) else {
                    return Err(Error::Provider(
                        "Anthropic event is missing id or name".into(),
                    ));
                };
                self.saw_tool = SawTool::Present;
                emit(ModelEvent::ToolCallStart {
                    index,
                    call_id: super::truncated_call_id(call_id).to_owned(),
                    name: self.tool_names.restore(name),
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn content_block_delta(
        &mut self,
        value: &Value,
        emit: &mut dyn FnMut(ModelEvent),
    ) -> Result<()> {
        let index = required_index(value)?;
        let delta = value
            .get("delta")
            .and_then(object)
            .cloned()
            .unwrap_or_default();
        let delta_kind = required_type(&delta)?;
        self.open_implicit_block(&delta_kind, index, emit);
        match delta_kind.as_str() {
            "text_delta" => {
                if let Some(delta_text) = text(delta.get("text").unwrap_or(&Value::Null)) {
                    emit(ModelEvent::TextDelta(delta_text.to_owned()));
                }
            }
            "thinking_delta" => {
                if let Some(delta_text) = text(delta.get("thinking").unwrap_or(&Value::Null)) {
                    self.thinking_text
                        .entry(index)
                        .or_default()
                        .push_str(delta_text);
                    emit(ModelEvent::ThinkingDelta(delta_text.to_owned()));
                }
            }
            "signature_delta" => {
                if let Some(signature) = text(delta.get("signature").unwrap_or(&Value::Null)) {
                    self.thinking_signatures
                        .entry(index)
                        .or_default()
                        .push_str(signature);
                }
            }
            "input_json_delta" => {
                if let Some(partial) = text(delta.get("partial_json").unwrap_or(&Value::Null)) {
                    self.saw_tool = SawTool::Present;
                    emit(ModelEvent::ToolCallArgumentsDelta {
                        index,
                        delta: partial.to_owned(),
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Some gateways skip `content_block_start` and open straight into a delta.
    fn open_implicit_block(
        &mut self,
        delta_kind: &str,
        index: usize,
        emit: &mut dyn FnMut(ModelEvent),
    ) {
        let kind = match delta_kind {
            "text_delta" => "text",
            "thinking_delta" | "signature_delta" => "thinking",
            _ => return,
        };
        if let std::collections::btree_map::Entry::Vacant(entry) = self.block_types.entry(index) {
            entry.insert(kind.into());
            if kind == "text" {
                emit(ModelEvent::TextStart);
            } else {
                self.thinking_text.insert(index, String::new());
                self.thinking_signatures.insert(index, String::new());
                emit(ModelEvent::ThinkingStart);
            }
        }
    }

    fn content_block_stop(
        &mut self,
        value: &Value,
        emit: &mut dyn FnMut(ModelEvent),
    ) -> Result<()> {
        let index = required_index(value)?;
        if let Some(kind) = self.block_types.remove(&index) {
            self.close_block(index, &kind, emit);
        }
        Ok(())
    }

    fn message_delta(&mut self, value: &Value) {
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            self.saw_usage = true;
            let update = anthropic_usage(usage);
            merge_usage(&mut self.usage, &update);
        }
        if let Some(reason) = value
            .get("delta")
            .and_then(object)
            .and_then(|delta| text(delta.get("stop_reason").unwrap_or(&Value::Null)))
        {
            self.finish = Some(map_stop(reason, self.saw_tool));
        }
    }

    fn finish_stream(&mut self, emit: &mut dyn FnMut(ModelEvent)) {
        self.close_remaining(emit);
        if !self.thinking_blocks.is_empty() {
            emit(ModelEvent::ProviderReplayState(
                crate::model::ProviderReplayState {
                    provider_kind: REPLAY_KIND.into(),
                    value: json!({ "blocks": self.thinking_blocks.clone() }),
                },
            ));
            self.thinking_blocks.clear();
        }
        if self.saw_usage {
            let context_input_tokens = self.usage.input.map(|input| {
                input
                    .saturating_add(self.usage.cache_read.unwrap_or_default())
                    .saturating_add(self.usage.cache_write.unwrap_or_default())
            });
            let total_tokens = self.usage.total.or_else(|| {
                context_input_tokens
                    .zip(self.usage.output)
                    .map(|(input, output)| input.saturating_add(output))
            });
            emit(ModelEvent::Usage(
                crate::model::Usage::new(self.usage.input, self.usage.output, total_tokens)
                    .with_context_input(context_input_tokens)
                    .with_cache_read(self.usage.cache_read)
                    .with_cache_write(self.usage.cache_write)
                    .with_reasoning(self.usage.reasoning),
            ));
        }
        emit(ModelEvent::Done(final_reason(self.finish, self.saw_tool)));
    }
}

impl ProtocolParser for Parser {
    fn push_data(
        &mut self,
        payload: &str,
        event: &str,
        emit: &mut dyn FnMut(ModelEvent),
    ) -> Result<bool> {
        if payload == "[DONE]" {
            return Ok(false);
        }
        let value = parse_object(payload, "Anthropic")?;
        if let Some(error) = event_error(&value) {
            return Err(Error::Provider(format!("Anthropic error: {error}")));
        }
        let kind = match event {
            "" | "message" => text(value.get("type").unwrap_or(&Value::Null)).unwrap_or(event),
            kind => kind,
        };
        match kind {
            "message_start" => self.message_start(&value),
            "content_block_start" => self.content_block_start(&value, emit)?,
            "content_block_delta" => self.content_block_delta(&value, emit)?,
            "content_block_stop" => self.content_block_stop(&value, emit)?,
            "message_delta" => self.message_delta(&value),
            "message_stop" => {
                self.terminal = true;
                self.finish_stream(emit);
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }

    fn finish(&mut self, emit: &mut dyn FnMut(ModelEvent)) -> Result<()> {
        if !self.terminal && self.finish.is_some() {
            self.terminal = true;
            self.finish_stream(emit);
        }
        if !self.terminal {
            return Err(Error::Provider(
                "Anthropic stream ended without message_stop".into(),
            ));
        }
        Ok(())
    }
}

pub(super) fn build_body(
    model: &str,
    invocation: &ModelInvocation,
    tool_names: &ToolNames,
) -> Result<Value> {
    let request = &invocation.request;
    let mut messages = anthropic_messages(&request.history, tool_names)?;
    mark_cache_breakpoint(&mut messages);
    let system = if request.prompt.instructions.is_empty() {
        Value::String(String::new())
    } else {
        json!([{
            "type": "text",
            "text": request.prompt.instructions,
            "cache_control": { "type": "ephemeral" },
        }])
    };
    let mut body = json!({
        "model": model,
        "system": system,
        "messages": messages,
        "max_tokens": request.model.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "stream": true,
    });
    let object = object_mut(&mut body)?;
    if !request.prompt.tools.is_empty() {
        object.insert(
            "tools".into(),
            request
                .prompt
                .tools
                .iter()
                .enumerate()
                .map(|(index, tool)| {
                    let mut value = json!({
                        "name": tool_names.alias(&tool.name),
                        "description": tool.description,
                        "input_schema": tool.parameters,
                    });
                    if index + 1 == request.prompt.tools.len()
                        && let Some(object) = value.as_object_mut()
                    {
                        object.insert("cache_control".into(), json!({ "type": "ephemeral" }));
                    }
                    value
                })
                .collect::<Value>(),
        );
    }
    if request.model.reasoning.is_enabled() {
        object.insert(
            "thinking".into(),
            json!({ "type": "adaptive", "display": "summarized" }),
        );
    }
    if let Some(effort) = request.model.reasoning.effort() {
        object.insert("output_config".into(), json!({ "effort": effort }));
    }
    super::merge_extra_params(&mut body, &request.model.extra_params)?;
    super::apply_stop_sequences(&mut body, "stop_sequences", &request.model.stop_sequences)?;
    Ok(body)
}

fn anthropic_parts(parts: &[ContentPart], allow_attachments: Attachments) -> Result<Vec<Value>> {
    let mut content = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text } => {
                if !text.is_empty() {
                    content.push(json!({ "type": "text", "text": text }));
                }
            }
            ContentPart::Image { .. } | ContentPart::File { .. }
                if allow_attachments == Attachments::Forbidden =>
            {
                return Err(Error::Provider(
                    "Anthropic only accepts attachments in user messages".into(),
                ));
            }
            ContentPart::Image { mime_type, data } => {
                content.push(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": mime_type,
                        "data": encode_base64(data),
                    },
                }));
            }
            ContentPart::File {
                mime_type,
                filename,
                data,
            } => {
                if mime_type == "application/pdf" {
                    content.push(json!({
                        "type": "document",
                        "source": {
                            "type": "base64",
                            "media_type": mime_type,
                            "data": encode_base64(data),
                        },
                    }));
                    continue;
                }
                if !mime_type.starts_with("text/") {
                    return Err(Error::Provider(
                        "Anthropic Messages only supports PDF and plain-text file attachments"
                            .into(),
                    ));
                }
                let mut document = json!({
                    "type": "document",
                    "source": {
                        "type": "plain_text",
                        "media_type": "text/plain",
                        "data": String::from_utf8_lossy(data),
                    },
                });
                if let Some(filename) = filename {
                    object_mut(&mut document)?.insert("title".into(), filename.clone().into());
                }
                content.push(document);
            }
        }
    }
    Ok(content)
}

fn replay_blocks(value: &Value) -> Vec<Value> {
    object(value)
        .and_then(|value| value.get("blocks"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn push_anthropic(output: &mut Vec<Value>, role: &str, content: Vec<Value>) {
    if let Some(last) = output.last_mut()
        && object(last)
            .is_some_and(|last| text(last.get("role").unwrap_or(&Value::Null)) == Some(role))
        && let Some(existing) = last
            .as_object_mut()
            .and_then(|last| last.get_mut("content"))
            .and_then(Value::as_array_mut)
    {
        existing.extend(content);
        return;
    }
    output.push(json!({ "role": role, "content": content }));
}

fn anthropic_messages(
    messages: &[crate::model::ProjectedMessage],
    tool_names: &ToolNames,
) -> Result<Vec<Value>> {
    let mut output = Vec::new();
    for message in messages {
        match core_message(message)? {
            CoreMessage::Assistant {
                text,
                replay_state,
                tool_calls,
            } => {
                let mut content = Vec::new();
                if let Some(state) = replay_state
                    && state.provider_kind == REPLAY_KIND
                {
                    content.extend(replay_blocks(&state.value));
                }
                if !text.is_empty() {
                    content.push(json!({ "type": "text", "text": text.as_ref() }));
                }
                for tool_call in tool_calls {
                    content.push(json!({
                        "type": "tool_use",
                        "id": super::truncated_call_id(&tool_call.call_id),
                        "name": tool_names.alias(&tool_call.name),
                        "input": tool_call.arguments,
                    }));
                }
                if !content.is_empty() {
                    push_anthropic(&mut output, "assistant", content);
                }
            }
            CoreMessage::Tool(result) => {
                push_anthropic(
                    &mut output,
                    "user",
                    vec![json!({
                        "type": "tool_result",
                        "tool_use_id": super::truncated_call_id(&result.call_id),
                        "content": if result.provider_parts.is_empty() {
                            Value::String(result.content.clone())
                        } else {
                            anthropic_parts(&result.provider_parts, Attachments::Allowed)?.into()
                        },
                    })],
                );
            }
            CoreMessage::System(content) => {
                let parts = anthropic_parts(content, Attachments::Forbidden)?;
                if !parts.is_empty() {
                    push_anthropic(&mut output, "user", parts);
                }
            }
            CoreMessage::User(content) => {
                let parts = anthropic_parts(content, Attachments::Allowed)?;
                if !parts.is_empty() {
                    push_anthropic(&mut output, "user", parts);
                }
            }
        }
    }
    Ok(output)
}

fn mark_cache_breakpoint(messages: &mut [Value]) {
    for message in messages.iter_mut().rev() {
        let Some(object) = message.as_object_mut() else {
            continue;
        };
        if text(object.get("role").unwrap_or(&Value::Null)) != Some("user") {
            continue;
        }
        let Some(content) = object.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in content.iter_mut().rev() {
            let Some(block) = block.as_object_mut() else {
                continue;
            };
            if let Some("text" | "image" | "document" | "tool_result") =
                text(block.get("type").unwrap_or(&Value::Null))
            {
                block.insert("cache_control".into(), json!({ "type": "ephemeral" }));
                return;
            }
        }
    }
}

/// Anthropic names the error class, so keep it in front of the message.
fn event_error(value: &Value) -> Option<String> {
    super::event_error_detail(value).map(|(kind, message)| match kind {
        Some(kind) => format!("{kind}: {message}"),
        None => message,
    })
}

fn empty_usage() -> UsageState {
    UsageState {
        input: None,
        output: None,
        total: None,
        cache_read: None,
        cache_write: None,
        reasoning: None,
    }
}

fn anthropic_usage(value: &Value) -> UsageState {
    let usage = object(value).cloned().unwrap_or_default();
    UsageState {
        input: usage.get("input_tokens").and_then(super::count_u64),
        output: usage.get("output_tokens").and_then(super::count_u64),
        total: usage.get("total_tokens").and_then(super::count_u64),
        cache_read: usage
            .get("cache_read_input_tokens")
            .and_then(super::count_u64),
        cache_write: usage
            .get("cache_creation_input_tokens")
            .and_then(super::count_u64),
        reasoning: None,
    }
}

fn merge_usage_field(total: Option<u64>, update: Option<u64>) -> Option<u64> {
    match update {
        None => total,
        Some(update) => Some(total.map_or(update, |total| total.max(update))),
    }
}

fn merge_usage(total: &mut UsageState, update: &UsageState) {
    total.input = merge_usage_field(total.input, update.input);
    total.output = merge_usage_field(total.output, update.output);
    total.total = merge_usage_field(total.total, update.total);
    total.cache_read = merge_usage_field(total.cache_read, update.cache_read);
    total.cache_write = merge_usage_field(total.cache_write, update.cache_write);
    let context = total.input.map(|input| {
        input.saturating_add(total.cache_read.unwrap_or(0)) + total.cache_write.unwrap_or(0)
    });
    if let (Some(context), Some(output)) = (context, total.output) {
        total.total = Some(context.saturating_add(output));
    }
}

fn map_stop(reason: &str, saw_tool: SawTool) -> FinishReason {
    match reason {
        "tool_use" => FinishReason::ToolUse,
        "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
        "end_turn" | "stop_sequence" | "pause_turn" | "refusal" => FinishReason::Stop,
        _ if saw_tool == SawTool::Present => FinishReason::ToolUse,
        _ => FinishReason::Stop,
    }
}

fn final_reason(finish: Option<FinishReason>, saw_tool: SawTool) -> FinishReason {
    if finish == Some(FinishReason::Length) {
        return FinishReason::Length;
    }
    if saw_tool == SawTool::Present {
        FinishReason::ToolUse
    } else {
        finish.unwrap_or(FinishReason::Stop)
    }
}

fn required_index(value: &Value) -> Result<usize> {
    value
        .get("index")
        .and_then(count_usize)
        .ok_or_else(|| Error::Provider("Anthropic event is missing index".into()))
}

fn required_type(value: &serde_json::Map<String, Value>) -> Result<String> {
    text(value.get("type").unwrap_or(&Value::Null))
        .map(str::to_owned)
        .ok_or_else(|| Error::Provider("Anthropic event is missing type".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{
            ContentPart, ProjectedContent, ProjectedMessage, ProviderReplayState, Reasoning, Role,
            ToolCallContent, ToolDefinition, ToolResultContent,
        },
        plugins::adapters::{ToolNames, collect_events, tests::invocation},
    };

    fn names(invocation: &crate::model::ModelInvocation) -> ToolNames {
        ToolNames::new(invocation)
    }

    #[test]
    fn marks_cache_breakpoints_and_defaults_max_tokens() {
        let mut invocation = invocation();
        invocation.request.prompt.tools = vec![ToolDefinition {
            name: "lookup".into(),
            description: "Look up".into(),
            parameters: json!({ "type": "object" }),
        }];
        invocation.request.model.reasoning = Reasoning::Effort("high".into());
        let body = build_body("claude-sonnet-4-5", &invocation, &names(&invocation)).unwrap();
        assert_eq!(body["max_tokens"], 65_000);
        assert_eq!(
            body["system"],
            json!([{
                "type": "text",
                "text": "You are concise.",
                "cache_control": { "type": "ephemeral" },
            }])
        );
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn replays_thinking_and_merges_roles() {
        let mut invocation = invocation();
        invocation.request.prompt.instructions.clear();
        invocation.request.history = vec![
            ProjectedMessage {
                message_id: "m1".into(),
                role: Role::System,
                content: ProjectedContent::Parts(vec![ContentPart::Text { text: "sys".into() }]),
            },
            ProjectedMessage {
                message_id: "m2".into(),
                role: Role::User,
                content: ProjectedContent::Parts(vec![ContentPart::Text { text: "ask".into() }]),
            },
            ProjectedMessage {
                message_id: "m3".into(),
                role: Role::Assistant,
                content: ProjectedContent::Assistant {
                    text: "ok".into(),
                    thinking: String::new(),
                    replay_state: Some(ProviderReplayState {
                        provider_kind: REPLAY_KIND.into(),
                        value: json!({
                            "blocks": [
                                { "type": "thinking", "thinking": "", "signature": "sig" },
                                { "type": "redacted_thinking", "data": "opaque" },
                            ]
                        }),
                    }),
                    calls: vec![ToolCallContent {
                        index: 0,
                        call_id: "call-1".into(),
                        name: "lookup".into(),
                        arguments: json!({ "q": "weather" }),
                    }],
                },
            },
            ProjectedMessage {
                message_id: "m4".into(),
                role: Role::Tool,
                content: ProjectedContent::ToolResult(ToolResultContent {
                    call_id: "call-1".into(),
                    name: "lookup".into(),
                    content: "sunny".into(),
                    is_error: false,
                    provider_parts: Vec::new(),
                }),
            },
            ProjectedMessage {
                message_id: "m5".into(),
                role: Role::User,
                content: ProjectedContent::Parts(vec![ContentPart::Text {
                    text: "thanks".into(),
                }]),
            },
        ];
        let body = build_body("claude-sonnet-4-5", &invocation, &names(&invocation)).unwrap();
        assert_eq!(body["system"], "");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(body["messages"][1]["content"][0]["type"], "thinking");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(
            body["messages"][2]["content"][1]["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[test]
    fn accepts_user_images_and_rejects_system_images() {
        let mut invocation = invocation();
        invocation.request.history = vec![ProjectedMessage {
            message_id: "m1".into(),
            role: Role::User,
            content: ProjectedContent::Parts(vec![
                ContentPart::Text { text: "see".into() },
                ContentPart::Image {
                    mime_type: "image/png".into(),
                    data: vec![0, 1, 2, 3],
                },
            ]),
        }];
        let body = build_body("claude-sonnet-4-5", &invocation, &names(&invocation)).unwrap();
        assert_eq!(body["messages"][0]["content"][1]["type"], "image");

        invocation.request.history = vec![ProjectedMessage {
            message_id: "m1".into(),
            role: Role::System,
            content: ProjectedContent::Parts(vec![ContentPart::Image {
                mime_type: "image/png".into(),
                data: vec![0, 1, 2, 3],
            }]),
        }];
        let error = build_body("claude-sonnet-4-5", &invocation, &names(&invocation)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Anthropic only accepts attachments in user messages"
        );
    }

    #[test]
    fn parses_text_thinking_usage_and_stop() {
        let events = collect_events(
            &mut Parser::new(names(&invocation())),
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":30}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"why"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hi"}}"#,
                r#"{"type":"content_block_stop","index":1}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        )
        .unwrap();
        assert_eq!(
            events,
            vec![
                ModelEvent::ThinkingStart,
                ModelEvent::ThinkingDelta("why".into()),
                ModelEvent::ThinkingEnd,
                ModelEvent::ThinkingSignature("sig".into()),
                ModelEvent::TextStart,
                ModelEvent::TextDelta("Hi".into()),
                ModelEvent::TextEnd,
                ModelEvent::ProviderReplayState(crate::model::ProviderReplayState {
                    provider_kind: REPLAY_KIND.into(),
                    value: json!({ "blocks": [{ "type": "thinking", "thinking": "why", "signature": "sig" }] }),
                }),
                ModelEvent::Usage(crate::model::Usage {
                    input_tokens: Some(10),
                    context_input_tokens: Some(60),
                    output_tokens: Some(2),
                    total_tokens: Some(62),
                    cache_read_tokens: Some(20),
                    cache_write_tokens: Some(30),
                    reasoning_tokens: None,
                }),
                ModelEvent::Done(FinishReason::Stop),
            ]
        );
    }

    #[test]
    fn replays_empty_thinking_with_signature_and_redacted() {
        let events = collect_events(
            &mut Parser::new(names(&invocation())),
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-only"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque"}}"#,
                r#"{"type":"content_block_stop","index":1}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        )
        .unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ProviderReplayState(state)
                if state.value["blocks"].as_array().unwrap().len() == 2
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ThinkingSignature(signature) if signature == "sig-only"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::RedactedThinking(data) if data == "opaque"
        )));
    }

    #[test]
    fn maps_tool_use_and_length() {
        let events = collect_events(
            &mut Parser::new(names(&invocation())),
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call-1","name":"lookup"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"a\"}"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        )
        .unwrap();
        assert_eq!(events.last(), Some(&ModelEvent::Done(FinishReason::Length)));
    }

    #[test]
    fn finishes_after_message_delta_without_stop() {
        let events = collect_events(
            &mut Parser::new(names(&invocation())),
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            ],
        )
        .unwrap();
        assert_eq!(
            events.last(),
            Some(&ModelEvent::Done(FinishReason::ToolUse))
        );
    }

    #[test]
    fn missing_message_stop_fails() {
        let error = collect_events(
            &mut Parser::new(names(&invocation())),
            &[r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#],
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Anthropic stream ended without message_stop"
        );
    }
}
