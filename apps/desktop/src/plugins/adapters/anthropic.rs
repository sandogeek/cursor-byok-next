//! Anthropic Messages request body and SSE → ModelEvent.

use serde_json::{Value, json};

use super::{ProtocolParser, count_usize, object, parse_object, text, usage_event};
use crate::{
    plugins::{
        Error, Result,
        wire::{LlmContentPart, LlmMessage, LlmRequest},
    },
    provider::{FinishReason, ModelEvent},
};

const REPLAY_KIND: &str = "anthropic";
const DEFAULT_MAX_TOKENS: u64 = 65_000;

struct UsageState {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
}

pub(super) struct Parser {
    block_types: std::collections::BTreeMap<usize, String>,
    thinking_text: std::collections::BTreeMap<usize, String>,
    thinking_signatures: std::collections::BTreeMap<usize, String>,
    thinking_blocks: Vec<Value>,
    usage: UsageState,
    saw_usage: bool,
    saw_tool: bool,
    finish: Option<FinishReason>,
    terminal: bool,
}

impl Parser {
    pub(super) fn new() -> Self {
        Self {
            block_types: std::collections::BTreeMap::new(),
            thinking_text: std::collections::BTreeMap::new(),
            thinking_signatures: std::collections::BTreeMap::new(),
            thinking_blocks: Vec::new(),
            usage: empty_usage(),
            saw_usage: false,
            saw_tool: false,
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
                if !signature.is_empty() {
                    self.thinking_blocks.push(json!({
                        "type": "thinking",
                        "thinking": thinking,
                        "signature": signature,
                    }));
                }
                emit(ModelEvent::ThinkingEnd);
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
            emit(usage_event(
                self.usage.input_tokens,
                self.usage.output_tokens,
                self.usage.total_tokens,
                self.usage.cache_read_tokens,
                self.usage.cache_write_tokens,
                self.usage.reasoning_tokens,
            ));
        }
        emit(ModelEvent::Done(final_reason(self.finish, self.saw_tool)));
    }
}

impl ProtocolParser for Parser {
    fn push_data(&mut self, payload: &str, emit: &mut dyn FnMut(ModelEvent)) -> Result<bool> {
        if payload == "[DONE]" {
            return Ok(false);
        }
        let value = parse_object(payload, "Anthropic")?;
        if let Some(error) = event_error(&value) {
            return Err(Error::Provider(format!("Anthropic error: {error}")));
        }
        match text(value.get("type").unwrap_or(&Value::Null)) {
            Some("message_start") => {
                if let Some(message_usage) = value
                    .get("message")
                    .and_then(object)
                    .and_then(|message| message.get("usage"))
                {
                    self.saw_usage = true;
                    merge_usage(&mut self.usage, anthropic_usage(message_usage));
                }
            }
            Some("content_block_start") => {
                let index = required_index(&value)?;
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
                        let data = text(block.get("data").unwrap_or(&Value::Null)).ok_or_else(
                            || Error::Provider("Anthropic event is missing data".into()),
                        )?;
                        self.thinking_blocks
                            .push(json!({ "type": "redacted_thinking", "data": data }));
                    }
                    "tool_use" => {
                        let call_id = text(block.get("id").unwrap_or(&Value::Null));
                        let name = text(block.get("name").unwrap_or(&Value::Null));
                        let (Some(call_id), Some(name)) = (call_id, name) else {
                            return Err(Error::Provider(
                                "Anthropic event is missing id or name".into(),
                            ));
                        };
                        self.saw_tool = true;
                        emit(ModelEvent::ToolCallStart {
                            index,
                            call_id: call_id.to_owned(),
                            name: name.to_owned(),
                        });
                    }
                    _ => {}
                }
            }
            Some("content_block_delta") => {
                let index = required_index(&value)?;
                let delta = value
                    .get("delta")
                    .and_then(object)
                    .cloned()
                    .unwrap_or_default();
                let delta_kind = required_type(&delta)?;
                if !self.block_types.contains_key(&index) {
                    if delta_kind == "text_delta" {
                        self.block_types.insert(index, "text".into());
                        emit(ModelEvent::TextStart);
                    } else if delta_kind == "thinking_delta" || delta_kind == "signature_delta" {
                        self.block_types.insert(index, "thinking".into());
                        self.thinking_text.insert(index, String::new());
                        self.thinking_signatures.insert(index, String::new());
                        emit(ModelEvent::ThinkingStart);
                    }
                }
                match delta_kind.as_str() {
                    "text_delta" => {
                        if let Some(delta_text) = text(delta.get("text").unwrap_or(&Value::Null)) {
                            emit(ModelEvent::TextDelta(delta_text.to_owned()));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(delta_text) =
                            text(delta.get("thinking").unwrap_or(&Value::Null))
                        {
                            self.thinking_text
                                .entry(index)
                                .or_default()
                                .push_str(delta_text);
                            emit(ModelEvent::ThinkingDelta(delta_text.to_owned()));
                        }
                    }
                    "signature_delta" => {
                        if let Some(signature) =
                            text(delta.get("signature").unwrap_or(&Value::Null))
                        {
                            self.thinking_signatures
                                .entry(index)
                                .or_default()
                                .push_str(signature);
                        }
                    }
                    "input_json_delta" => {
                        if let Some(partial) =
                            text(delta.get("partial_json").unwrap_or(&Value::Null))
                        {
                            self.saw_tool = true;
                            emit(ModelEvent::ToolCallArgumentsDelta {
                                index,
                                delta: partial.to_owned(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let index = required_index(&value)?;
                if let Some(kind) = self.block_types.remove(&index) {
                    self.close_block(index, &kind, emit);
                }
            }
            Some("message_delta") => {
                if !value.get("usage").is_none_or(Value::is_null) {
                    self.saw_usage = true;
                    merge_usage(
                        &mut self.usage,
                        anthropic_usage(value.get("usage").unwrap()),
                    );
                }
                if let Some(reason) = value
                    .get("delta")
                    .and_then(object)
                    .and_then(|delta| text(delta.get("stop_reason").unwrap_or(&Value::Null)))
                {
                    self.finish = Some(map_stop(reason, self.saw_tool));
                }
            }
            Some("message_stop") => {
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

pub(super) fn build_body(model: &str, request: &LlmRequest) -> Result<Value> {
    let mut messages = anthropic_messages(&request.messages)?;
    mark_cache_breakpoint(&mut messages);
    let system = if request.instructions.is_empty() {
        Value::String(String::new())
    } else {
        json!([{
            "type": "text",
            "text": request.instructions,
            "cache_control": { "type": "ephemeral" },
        }])
    };
    let mut body = json!({
        "model": model,
        "system": system,
        "messages": messages,
        "max_tokens": request.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "stream": true,
    });
    let object = body.as_object_mut().unwrap();
    if !request.tools.is_empty() {
        object.insert(
            "tools".into(),
            request
                .tools
                .iter()
                .enumerate()
                .map(|(index, tool)| {
                    let mut value = json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.parameters,
                    });
                    if index + 1 == request.tools.len() {
                        value.as_object_mut().unwrap().insert(
                            "cache_control".into(),
                            json!({ "type": "ephemeral" }),
                        );
                    }
                    value
                })
                .collect::<Value>(),
        );
    }
    if request.reasoning.enabled {
        object.insert(
            "thinking".into(),
            json!({ "type": "adaptive", "display": "summarized" }),
        );
    }
    if let Some(effort) = &request.reasoning.effort {
        object.insert("output_config".into(), json!({ "effort": effort }));
    }
    Ok(body)
}

fn anthropic_parts(parts: &[LlmContentPart], allow_attachments: bool) -> Result<Vec<Value>> {
    let mut content = Vec::new();
    for part in parts {
        match part {
            LlmContentPart::Text { text } => {
                if !text.is_empty() {
                    content.push(json!({ "type": "text", "text": text }));
                }
            }
            LlmContentPart::Image {
                media_type,
                data_base64,
            } => {
                if !allow_attachments {
                    return Err(Error::Provider(
                        "Anthropic only accepts attachments in user messages".into(),
                    ));
                }
                content.push(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": media_type,
                        "data": data_base64,
                    },
                }));
            }
            LlmContentPart::File {
                media_type,
                filename,
                data_base64,
            } => {
                if !allow_attachments {
                    return Err(Error::Provider(
                        "Anthropic only accepts attachments in user messages".into(),
                    ));
                }
                if media_type == "application/pdf" {
                    content.push(json!({
                        "type": "document",
                        "source": {
                            "type": "base64",
                            "media_type": media_type,
                            "data": data_base64,
                        },
                    }));
                    continue;
                }
                if !media_type.starts_with("text/") {
                    return Err(Error::Provider(
                        "Anthropic Messages only supports PDF and plain-text file attachments"
                            .into(),
                    ));
                }
                let decoded = base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    data_base64,
                )
                .map_err(|error| Error::Provider(format!("invalid Anthropic text attachment: {error}")))?;
                let mut document = json!({
                    "type": "document",
                    "source": {
                        "type": "plain_text",
                        "media_type": "text/plain",
                        "data": String::from_utf8_lossy(&decoded),
                    },
                });
                if let Some(filename) = filename {
                    document
                        .as_object_mut()
                        .unwrap()
                        .insert("title".into(), filename.clone().into());
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
        && object(last).is_some_and(|last| text(last.get("role").unwrap_or(&Value::Null)) == Some(role))
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

fn anthropic_messages(messages: &[LlmMessage]) -> Result<Vec<Value>> {
    let mut output = Vec::new();
    for message in messages {
        match message {
            LlmMessage::Assistant {
                text,
                replay_state,
                tool_calls,
                ..
            } => {
                let mut content = Vec::new();
                if let Some(state) = replay_state
                    && state.provider_kind == REPLAY_KIND
                {
                    content.extend(replay_blocks(&state.value));
                }
                if !text.is_empty() {
                    content.push(json!({ "type": "text", "text": text }));
                }
                for tool_call in tool_calls {
                    content.push(json!({
                        "type": "tool_use",
                        "id": tool_call.call_id,
                        "name": tool_call.name,
                        "input": tool_call.arguments,
                    }));
                }
                if !content.is_empty() {
                    push_anthropic(&mut output, "assistant", content);
                }
            }
            LlmMessage::Tool {
                call_id,
                content,
                parts,
                ..
            } => {
                push_anthropic(
                    &mut output,
                    "user",
                    vec![json!({
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": if parts.is_empty() {
                            Value::String(content.clone())
                        } else {
                            anthropic_parts(parts, true)?.into()
                        },
                    })],
                );
            }
            LlmMessage::System { content } | LlmMessage::User { content } => {
                let allow = matches!(message, LlmMessage::User { .. });
                let parts = anthropic_parts(content, allow)?;
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
            match text(block.get("type").unwrap_or(&Value::Null)) {
                Some("text" | "image" | "document" | "tool_result") => {
                    block.insert("cache_control".into(), json!({ "type": "ephemeral" }));
                    return;
                }
                _ => {}
            }
        }
    }
}

fn event_error(value: &Value) -> Option<String> {
    let kind = text(value.get("type").unwrap_or(&Value::Null));
    if kind != Some("error") && value.get("error").is_none_or(Value::is_null) {
        return None;
    }
    let error = value.get("error").unwrap_or(&Value::Null);
    if let Some(text) = error.as_str() {
        return Some(text.to_owned());
    }
    let details = object(error);
    let kind = details.and_then(|details| text(details.get("type").unwrap_or(&Value::Null)));
    let message = details
        .and_then(|details| text(details.get("message").unwrap_or(&Value::Null)))
        .or_else(|| text(value.get("message").unwrap_or(&Value::Null)));
    match (kind, message) {
        (Some(kind), Some(message)) => Some(format!("{kind}: {message}")),
        (None, Some(message)) => Some(message.to_owned()),
        (Some(kind), None) => Some(kind.to_owned()),
        (None, None) => Some(error.to_string()),
    }
}

fn empty_usage() -> UsageState {
    UsageState {
        input_tokens: None,
        output_tokens: None,
        total_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        reasoning_tokens: None,
    }
}

fn anthropic_usage(value: &Value) -> UsageState {
    let usage = object(value).cloned().unwrap_or_default();
    UsageState {
        input_tokens: usage.get("input_tokens").and_then(super::count_u64),
        output_tokens: usage.get("output_tokens").and_then(super::count_u64),
        total_tokens: usage.get("total_tokens").and_then(super::count_u64),
        cache_read_tokens: usage
            .get("cache_read_input_tokens")
            .and_then(super::count_u64),
        cache_write_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(super::count_u64),
        reasoning_tokens: None,
    }
}

fn merge_usage_field(total: Option<u64>, update: Option<u64>) -> Option<u64> {
    match update {
        None => total,
        Some(update) => Some(total.map_or(update, |total| total.max(update))),
    }
}

fn merge_usage(total: &mut UsageState, update: UsageState) {
    total.input_tokens = merge_usage_field(total.input_tokens, update.input_tokens);
    total.output_tokens = merge_usage_field(total.output_tokens, update.output_tokens);
    total.total_tokens = merge_usage_field(total.total_tokens, update.total_tokens);
    total.cache_read_tokens = merge_usage_field(total.cache_read_tokens, update.cache_read_tokens);
    total.cache_write_tokens = merge_usage_field(total.cache_write_tokens, update.cache_write_tokens);
    let context = total.input_tokens.map(|input| {
        input + total.cache_read_tokens.unwrap_or(0) + total.cache_write_tokens.unwrap_or(0)
    });
    if let (Some(context), Some(output)) = (context, total.output_tokens) {
        total.total_tokens = Some(context + output);
    }
}

fn map_stop(reason: &str, saw_tool: bool) -> FinishReason {
    match reason {
        "tool_use" => FinishReason::ToolUse,
        "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
        "end_turn" | "stop_sequence" | "pause_turn" | "refusal" => FinishReason::Stop,
        _ => {
            if saw_tool {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            }
        }
    }
}

fn final_reason(finish: Option<FinishReason>, saw_tool: bool) -> FinishReason {
    if finish == Some(FinishReason::Length) {
        return FinishReason::Length;
    }
    if saw_tool {
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
    use crate::plugins::{
        adapters::{collect_events, tests::request},
        wire::{LlmContentPart, LlmMessage, LlmReplayState, LlmTool, LlmToolCall},
    };

    #[test]
    fn marks_cache_breakpoints_and_defaults_max_tokens() {
        let mut request = request();
        request.tools = vec![LlmTool {
            name: "lookup".into(),
            description: "Look up".into(),
            parameters: json!({ "type": "object" }),
        }];
        request.reasoning.enabled = true;
        request.reasoning.effort = Some("high".into());
        let body = build_body("claude-sonnet-4-5", &request).unwrap();
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
        let mut request = request();
        request.instructions.clear();
        request.messages = vec![
            LlmMessage::System {
                content: vec![LlmContentPart::Text { text: "sys".into() }],
            },
            LlmMessage::User {
                content: vec![LlmContentPart::Text { text: "ask".into() }],
            },
            LlmMessage::Assistant {
                text: "ok".into(),
                thinking: String::new(),
                replay_state: Some(LlmReplayState {
                    provider_kind: REPLAY_KIND.into(),
                    value: json!({
                        "blocks": [
                            { "type": "thinking", "thinking": "", "signature": "sig" },
                            { "type": "redacted_thinking", "data": "opaque" },
                        ]
                    }),
                }),
                tool_calls: vec![LlmToolCall {
                    index: 0,
                    call_id: "call-1".into(),
                    name: "lookup".into(),
                    arguments: json!({ "q": "weather" }),
                }],
            },
            LlmMessage::Tool {
                call_id: "call-1".into(),
                name: "lookup".into(),
                content: "sunny".into(),
                is_error: false,
                parts: Vec::new(),
            },
            LlmMessage::User {
                content: vec![LlmContentPart::Text {
                    text: "thanks".into(),
                }],
            },
        ];
        let body = build_body("claude-sonnet-4-5", &request).unwrap();
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
        let mut request = request();
        request.messages = vec![LlmMessage::User {
            content: vec![
                LlmContentPart::Text { text: "see".into() },
                LlmContentPart::Image {
                    media_type: "image/png".into(),
                    data_base64: "AAECAw==".into(),
                },
            ],
        }];
        let body = build_body("claude-sonnet-4-5", &request).unwrap();
        assert_eq!(body["messages"][0]["content"][1]["type"], "image");

        request.messages = vec![LlmMessage::System {
            content: vec![LlmContentPart::Image {
                media_type: "image/png".into(),
                data_base64: "AAECAw==".into(),
            }],
        }];
        let error = build_body("claude-sonnet-4-5", &request).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Anthropic only accepts attachments in user messages"
        );
    }

    #[test]
    fn parses_text_thinking_usage_and_stop() {
        let events = collect_events(
            &mut Parser::new(),
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
                ModelEvent::TextStart,
                ModelEvent::TextDelta("Hi".into()),
                ModelEvent::TextEnd,
                ModelEvent::ProviderReplayState(crate::model::ProviderReplayState {
                    provider_kind: REPLAY_KIND.into(),
                    value: json!({ "blocks": [{ "type": "thinking", "thinking": "why", "signature": "sig" }] }),
                }),
                ModelEvent::Usage(crate::model::Usage {
                    input_tokens: Some(10),
                    context_input_tokens: Some(10),
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
            &mut Parser::new(),
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
    }

    #[test]
    fn maps_tool_use_and_length() {
        let events = collect_events(
            &mut Parser::new(),
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
            &mut Parser::new(),
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
            &mut Parser::new(),
            &[r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#],
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Anthropic stream ended without message_stop"
        );
    }
}
