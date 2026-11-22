//! OpenAI Responses request body and SSE → ModelEvent.

use serde_json::{Value, json};

use super::{ProtocolParser, count_usize, object, parse_object, text, usage_event};
use crate::{
    plugins::{
        Error, Result,
        wire::{LlmContentPart, LlmMessage, LlmRequest},
    },
    provider::{FinishReason, ModelEvent},
};

const REPLAY_KIND: &str = "openai_responses";

struct ToolState {
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
    emitted: usize,
    started: bool,
    ended: bool,
}

enum ToolArguments {
    None,
    Delta(String),
    Snapshot(String),
}

pub(super) struct Parser {
    text_open: bool,
    streamed_text: String,
    thinking_open: bool,
    tools: std::collections::BTreeMap<usize, ToolState>,
    reasoning_items: Vec<Value>,
    saw_tool: bool,
    saw_completed_item: bool,
    terminal: bool,
}

impl Parser {
    pub(super) fn new() -> Self {
        Self {
            text_open: false,
            streamed_text: String::new(),
            thinking_open: false,
            tools: std::collections::BTreeMap::new(),
            reasoning_items: Vec::new(),
            saw_tool: false,
            saw_completed_item: false,
            terminal: false,
        }
    }

    fn close_thinking(&mut self, emit: &mut dyn FnMut(ModelEvent)) {
        if self.thinking_open {
            self.thinking_open = false;
            emit(ModelEvent::ThinkingEnd);
        }
    }

    fn close_text(&mut self, emit: &mut dyn FnMut(ModelEvent)) {
        if self.text_open {
            self.text_open = false;
            emit(ModelEvent::TextEnd);
        }
    }

    fn reconcile_text(&mut self, final_text: &str, emit: &mut dyn FnMut(ModelEvent)) {
        if final_text.starts_with(&self.streamed_text) && final_text.len() > self.streamed_text.len()
        {
            if !self.text_open {
                self.text_open = true;
                emit(ModelEvent::TextStart);
            }
            emit(ModelEvent::TextDelta(
                final_text[self.streamed_text.len()..].to_owned(),
            ));
            self.streamed_text = final_text.to_owned();
        }
    }

    fn end_started_tools(&mut self, emit: &mut dyn FnMut(ModelEvent)) {
        for (index, tool) in &mut self.tools {
            if tool.started && !tool.ended {
                tool.ended = true;
                emit(ModelEvent::ToolCallEnd { index: *index });
            }
        }
    }

    fn emit_replay_state(&mut self, emit: &mut dyn FnMut(ModelEvent)) {
        if !self.reasoning_items.is_empty() {
            emit(ModelEvent::ProviderReplayState(
                crate::model::ProviderReplayState {
                    provider_kind: REPLAY_KIND.into(),
                    value: json!({ "items": self.reasoning_items.clone() }),
                },
            ));
            self.reasoning_items.clear();
        }
    }
}

impl ProtocolParser for Parser {
    fn push_data(&mut self, payload: &str, emit: &mut dyn FnMut(ModelEvent)) -> Result<bool> {
        if payload == "[DONE]" {
            return Ok(true);
        }
        let value = parse_object(payload, "OpenAI Responses")?;
        match text(value.get("type").unwrap_or(&Value::Null)) {
            Some("response.output_text.delta") => {
                self.close_thinking(emit);
                if !self.text_open {
                    self.text_open = true;
                    emit(ModelEvent::TextStart);
                }
                if let Some(delta) = text(value.get("delta").unwrap_or(&Value::Null)) {
                    self.streamed_text.push_str(delta);
                    emit(ModelEvent::TextDelta(delta.to_owned()));
                }
            }
            Some("response.output_text.done") => {
                if let Some(final_text) = text(value.get("text").unwrap_or(&Value::Null)) {
                    self.reconcile_text(final_text, emit);
                }
                self.close_text(emit);
            }
            Some("response.reasoning_summary_text.delta" | "response.reasoning_text.delta") => {
                if !self.thinking_open {
                    self.thinking_open = true;
                    emit(ModelEvent::ThinkingStart);
                }
                if let Some(delta) = text(value.get("delta").unwrap_or(&Value::Null)) {
                    emit(ModelEvent::ThinkingDelta(delta.to_owned()));
                }
            }
            Some("response.reasoning_summary_text.done" | "response.reasoning_text.done") => {
                self.close_thinking(emit);
            }
            Some("response.output_item.added") => {
                let item = value.get("item").and_then(object);
                if item.and_then(|item| text(item.get("type").unwrap_or(&Value::Null)))
                    != Some("function_call")
                {
                    return Ok(false);
                }
                self.saw_tool = true;
                for event in update_tool(
                    required_index(&value)?,
                    item,
                    ToolArguments::None,
                    false,
                    &mut self.tools,
                )? {
                    emit(event);
                }
            }
            Some("response.output_item.done") => {
                let item = value.get("item").and_then(object);
                match item.and_then(|item| text(item.get("type").unwrap_or(&Value::Null))) {
                    Some("reasoning") => {
                        self.close_thinking(emit);
                        self.reasoning_items
                            .push(value.get("item").cloned().unwrap_or(Value::Null));
                    }
                    Some("message") => {
                        self.saw_completed_item = true;
                        if let Some(item) = item
                            && let Some(final_text) = item_text(item)
                        {
                            self.reconcile_text(&final_text, emit);
                        }
                        self.close_text(emit);
                    }
                    Some("function_call") => {
                        self.saw_completed_item = true;
                        self.saw_tool = true;
                        let snapshot = item.and_then(|item| {
                            text(item.get("arguments").unwrap_or(&Value::Null))
                        });
                        let args = match snapshot {
                            None => ToolArguments::None,
                            Some(snapshot) => ToolArguments::Snapshot(snapshot.to_owned()),
                        };
                        for event in update_tool(
                            required_index(&value)?,
                            item,
                            args,
                            true,
                            &mut self.tools,
                        )? {
                            emit(event);
                        }
                    }
                    _ => {}
                }
            }
            Some("response.function_call_arguments.delta") => {
                let Some(delta) = text(value.get("delta").unwrap_or(&Value::Null)) else {
                    return Ok(false);
                };
                self.saw_tool = true;
                for event in update_tool(
                    required_index(&value)?,
                    None,
                    ToolArguments::Delta(delta.to_owned()),
                    false,
                    &mut self.tools,
                )? {
                    emit(event);
                }
            }
            Some("response.function_call_arguments.done") => {
                let snapshot = text(value.get("arguments").unwrap_or(&Value::Null));
                let args = match snapshot {
                    None | Some("") => ToolArguments::None,
                    Some(snapshot) => ToolArguments::Snapshot(snapshot.to_owned()),
                };
                let done = snapshot.is_some_and(|snapshot| !snapshot.is_empty());
                for event in
                    update_tool(required_index(&value)?, None, args, done, &mut self.tools)?
                {
                    emit(event);
                }
            }
            Some("response.completed") => {
                if let Some(usage) = value
                    .get("response")
                    .and_then(object)
                    .and_then(|response| response.get("usage"))
                {
                    emit(responses_usage(usage));
                }
                self.close_thinking(emit);
                self.close_text(emit);
                self.end_started_tools(emit);
                if self.tools.values().any(|tool| !tool.started) {
                    return Err(Error::Provider(
                        "OpenAI Responses completed with incomplete tool metadata".into(),
                    ));
                }
                self.terminal = true;
                self.emit_replay_state(emit);
                emit(ModelEvent::Done(if self.saw_tool {
                    FinishReason::ToolUse
                } else {
                    FinishReason::Stop
                }));
                return Ok(true);
            }
            Some("response.incomplete") => {
                self.close_thinking(emit);
                self.close_text(emit);
                self.end_started_tools(emit);
                self.terminal = true;
                emit(ModelEvent::Done(FinishReason::Length));
                return Ok(true);
            }
            Some("response.failed") => {
                return Err(Error::Provider(format!(
                    "OpenAI Responses failed: {payload}"
                )));
            }
            _ => {}
        }
        Ok(false)
    }

    fn finish(&mut self, emit: &mut dyn FnMut(ModelEvent)) -> Result<()> {
        if !self.terminal && self.saw_completed_item {
            self.close_thinking(emit);
            self.close_text(emit);
            if self.tools.values().any(|tool| !tool.ended) {
                return Err(Error::Provider(
                    "OpenAI Responses stream ended with an incomplete tool call".into(),
                ));
            }
            self.terminal = true;
            self.emit_replay_state(emit);
            emit(ModelEvent::Done(if self.saw_tool {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            }));
        }
        if !self.terminal {
            return Err(Error::Provider(
                "OpenAI Responses stream ended without response.completed or response.incomplete"
                    .into(),
            ));
        }
        Ok(())
    }
}

pub(super) fn build_body(model: &str, request: &LlmRequest) -> Result<Value> {
    let mut input = Vec::new();
    for message in &request.messages {
        match message {
            LlmMessage::Assistant {
                text,
                replay_state,
                tool_calls,
                ..
            } => {
                if let Some(state) = replay_state
                    && state.provider_kind == REPLAY_KIND
                {
                    input.extend(replay_items(&state.value)?);
                }
                if !text.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                for tool_call in tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": tool_call.call_id,
                        "name": tool_call.name,
                        "arguments": serde_json::to_string(&tool_call.arguments).unwrap_or_else(|_| "{}".into()),
                    }));
                }
            }
            LlmMessage::Tool {
                call_id,
                content,
                parts,
                ..
            } => {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": if parts.is_empty() {
                        Value::String(content.clone())
                    } else {
                        content_parts(parts, "input_text")?.into()
                    },
                }));
            }
            LlmMessage::System { content } | LlmMessage::User { content } => {
                let role = if matches!(message, LlmMessage::System { .. }) {
                    "system"
                } else {
                    "user"
                };
                let parts = content_parts(content, "input_text")?;
                if !parts.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": role,
                        "content": parts,
                    }));
                }
            }
        }
    }
    let mut body = json!({
        "model": model,
        "input": input,
        "stream": true,
        "instructions": request.instructions,
        "include": ["reasoning.encrypted_content"],
    });
    let object = body.as_object_mut().unwrap();
    if !request.tools.is_empty() {
        object.insert(
            "tools".into(),
            request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                        "strict": false,
                    })
                })
                .collect::<Value>(),
        );
    }
    if let Some(tokens) = request.max_output_tokens {
        object.insert("max_output_tokens".into(), tokens.into());
    }
    if request.reasoning.enabled || request.reasoning.effort.is_some() {
        let mut reasoning = json!({ "summary": "auto" });
        if let Some(effort) = &request.reasoning.effort {
            reasoning
                .as_object_mut()
                .unwrap()
                .insert("effort".into(), effort.clone().into());
        }
        object.insert("reasoning".into(), reasoning);
    }
    if request.latency == "fast" {
        object.insert("service_tier".into(), "priority".into());
    }
    if let Some(cache_key) = &request.cache_key {
        object.insert("prompt_cache_key".into(), cache_key.clone().into());
    }
    Ok(body)
}

fn content_parts(parts: &[LlmContentPart], text_type: &str) -> Result<Vec<Value>> {
    let mut content = Vec::new();
    for part in parts {
        match part {
            LlmContentPart::Text { text } => {
                if !text.is_empty() {
                    content.push(json!({ "type": text_type, "text": text }));
                }
            }
            LlmContentPart::Image {
                media_type,
                data_base64,
            } => content.push(json!({
                "type": "input_image",
                "detail": "auto",
                "image_url": format!("data:{media_type};base64,{data_base64}"),
            })),
            LlmContentPart::File {
                media_type,
                filename,
                data_base64,
            } => {
                if text_type != "input_text" {
                    return Err(Error::Provider(
                        "OpenAI Responses does not accept files in assistant messages".into(),
                    ));
                }
                content.push(json!({
                    "type": "input_file",
                    "filename": filename.clone().unwrap_or_else(|| "attachment".into()),
                    "file_data": format!("data:{media_type};base64,{data_base64}"),
                }));
            }
        }
    }
    Ok(content)
}

fn replay_items(value: &Value) -> Result<Vec<Value>> {
    let items = object(value)
        .and_then(|value| value.get("items"))
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Provider("OpenAI Responses replay state is missing items".into()))?;
    items
        .iter()
        .map(|item| {
            let source = object(item).ok_or_else(|| {
                Error::Provider("OpenAI Responses replay state contains a non-reasoning item".into())
            })?;
            if text(source.get("type").unwrap_or(&Value::Null)) != Some("reasoning") {
                return Err(Error::Provider(
                    "OpenAI Responses replay state contains a non-reasoning item".into(),
                ));
            }
            let mut projected = json!({ "type": "reasoning" });
            for field in ["id", "summary", "content", "encrypted_content"] {
                if let Some(value) = source.get(field) {
                    projected
                        .as_object_mut()
                        .unwrap()
                        .insert(field.into(), value.clone());
                }
            }
            Ok(projected)
        })
        .collect()
}

fn update_tool(
    index: usize,
    item: Option<&serde_json::Map<String, Value>>,
    args: ToolArguments,
    done: bool,
    tools: &mut std::collections::BTreeMap<usize, ToolState>,
) -> Result<Vec<ModelEvent>> {
    let tool = tools.entry(index).or_insert_with(|| ToolState {
        call_id: None,
        name: None,
        arguments: String::new(),
        emitted: 0,
        started: false,
        ended: false,
    });
    if tool.call_id.is_none() {
        tool.call_id = item.and_then(|item| text(item.get("call_id").unwrap_or(&Value::Null)))
            .map(str::to_owned);
    }
    if tool.name.is_none() {
        tool.name = item
            .and_then(|item| text(item.get("name").unwrap_or(&Value::Null)))
            .map(str::to_owned);
    }
    match args {
        ToolArguments::None => {}
        ToolArguments::Delta(delta) => tool.arguments.push_str(&delta),
        ToolArguments::Snapshot(snapshot) if snapshot != tool.arguments => {
            if !snapshot.starts_with(&tool.arguments) {
                return Err(Error::Provider(
                    "OpenAI Responses final tool arguments do not match streamed arguments".into(),
                ));
            }
            tool.arguments
                .push_str(&snapshot[tool.arguments.len()..]);
        }
        ToolArguments::Snapshot(_) => {}
    }
    let mut events = Vec::new();
    if !tool.started && tool.call_id.is_some() && tool.name.is_some() {
        tool.started = true;
        events.push(ModelEvent::ToolCallStart {
            index,
            call_id: tool.call_id.clone().unwrap(),
            name: tool.name.clone().unwrap(),
        });
    }
    if tool.started && tool.emitted < tool.arguments.len() {
        events.push(ModelEvent::ToolCallArgumentsDelta {
            index,
            delta: tool.arguments[tool.emitted..].to_owned(),
        });
        tool.emitted = tool.arguments.len();
    }
    if done && !tool.ended {
        if !tool.started {
            return Err(Error::Provider(
                "OpenAI Responses function call is missing call_id or name".into(),
            ));
        }
        tool.ended = true;
        events.push(ModelEvent::ToolCallEnd { index });
    }
    Ok(events)
}

fn item_text(item: &serde_json::Map<String, Value>) -> Option<String> {
    let content = item.get("content")?.as_array()?;
    Some(
        content
            .iter()
            .filter_map(object)
            .filter(|part| text(part.get("type").unwrap_or(&Value::Null)) == Some("output_text"))
            .map(|part| text(part.get("text").unwrap_or(&Value::Null)).unwrap_or(""))
            .collect(),
    )
}

fn required_index(value: &Value) -> Result<usize> {
    value
        .get("output_index")
        .and_then(count_usize)
        .ok_or_else(|| Error::Provider("OpenAI Responses event is missing output_index".into()))
}

fn responses_usage(value: &Value) -> ModelEvent {
    let usage = object(value).cloned().unwrap_or_default();
    let input_details = usage.get("input_tokens_details").and_then(object);
    let output_details = usage.get("output_tokens_details").and_then(object);
    usage_event(
        usage.get("input_tokens").and_then(super::count_u64),
        usage.get("output_tokens").and_then(super::count_u64),
        usage.get("total_tokens").and_then(super::count_u64),
        input_details.and_then(|details| details.get("cached_tokens").and_then(super::count_u64)),
        None,
        output_details
            .and_then(|details| details.get("reasoning_tokens").and_then(super::count_u64)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::{
        adapters::{collect_events, tests::request},
        wire::{LlmContentPart, LlmMessage, LlmReplayState, LlmTool, LlmToolCall},
    };

    #[test]
    fn builds_responses_body_with_priority_tier() {
        let mut request = request();
        request.latency = "fast".into();
        request.reasoning.enabled = true;
        request.reasoning.effort = Some("high".into());
        request.tools = vec![LlmTool {
            name: "lookup".into(),
            description: "Look up".into(),
            parameters: json!({ "type": "object" }),
        }];
        let body = build_body("gpt-5", &request).unwrap();
        assert_eq!(body["instructions"], "You are concise.");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["prompt_cache_key"], "conversation-1");
        assert_eq!(body["tools"][0]["strict"], false);
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    }

    #[test]
    fn replays_only_reasoning_items() {
        let mut request = request();
        request.instructions.clear();
        request.messages = vec![LlmMessage::Assistant {
            text: "ok".into(),
            thinking: String::new(),
            replay_state: Some(LlmReplayState {
                provider_kind: REPLAY_KIND.into(),
                value: json!({
                    "items": [{
                        "type": "reasoning",
                        "id": "r1",
                        "encrypted_content": "enc"
                    }]
                }),
            }),
            tool_calls: vec![LlmToolCall {
                index: 0,
                call_id: "call-1".into(),
                name: "lookup".into(),
                arguments: json!({ "q": "a" }),
            }],
        }];
        let body = build_body("gpt-5", &request).unwrap();
        assert_eq!(body["input"][0]["type"], "reasoning");
        assert_eq!(body["input"][0]["encrypted_content"], "enc");
        assert_eq!(body["input"][1]["type"], "message");
        assert_eq!(body["input"][2]["type"], "function_call");
    }

    #[test]
    fn rejects_non_reasoning_replay() {
        let mut request = request();
        request.messages = vec![LlmMessage::Assistant {
            text: String::new(),
            thinking: String::new(),
            replay_state: Some(LlmReplayState {
                provider_kind: REPLAY_KIND.into(),
                value: json!({ "items": [{ "type": "message" }] }),
            }),
            tool_calls: Vec::new(),
        }];
        let error = build_body("gpt-5", &request).unwrap_err();
        assert_eq!(
            error.to_string(),
            "OpenAI Responses replay state contains a non-reasoning item"
        );
    }

    #[test]
    fn serializes_files_and_images() {
        let mut request = request();
        request.instructions.clear();
        request.messages = vec![LlmMessage::User {
            content: vec![
                LlmContentPart::Image {
                    media_type: "image/png".into(),
                    data_base64: "AA==".into(),
                },
                LlmContentPart::File {
                    media_type: "application/pdf".into(),
                    filename: Some("report.pdf".into()),
                    data_base64: "JVBERi0x".into(),
                },
            ],
        }];
        let body = build_body("gpt-5", &request).unwrap();
        assert_eq!(body["input"][0]["content"][0]["type"], "input_image");
        assert_eq!(body["input"][0]["content"][1]["type"], "input_file");
        assert_eq!(body["input"][0]["content"][1]["filename"], "report.pdf");
    }

    #[test]
    fn parses_text_thinking_usage_and_completed() {
        let events = collect_events(
            &mut Parser::new(),
            &[
                r#"{"type":"response.reasoning_summary_text.delta","delta":"why"}"#,
                r#"{"type":"response.reasoning_summary_text.done"}"#,
                r#"{"type":"response.output_text.delta","delta":"Hi"}"#,
                r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"enc"}}"#,
                r#"{"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5,"input_tokens_details":{"cached_tokens":1},"output_tokens_details":{"reasoning_tokens":4}}}}"#,
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
                ModelEvent::Usage(crate::model::Usage {
                    input_tokens: Some(3),
                    context_input_tokens: Some(3),
                    output_tokens: Some(2),
                    total_tokens: Some(5),
                    cache_read_tokens: Some(1),
                    cache_write_tokens: None,
                    reasoning_tokens: Some(4),
                }),
                ModelEvent::TextEnd,
                ModelEvent::ProviderReplayState(crate::model::ProviderReplayState {
                    provider_kind: REPLAY_KIND.into(),
                    value: json!({ "items": [{ "type": "reasoning", "encrypted_content": "enc" }] }),
                }),
                ModelEvent::Done(FinishReason::Stop),
            ]
        );
    }

    #[test]
    fn reconciles_output_text_suffix() {
        let events = collect_events(
            &mut Parser::new(),
            &[
                r#"{"type":"response.output_text.delta","delta":"Hel"}"#,
                r#"{"type":"response.output_text.done","text":"Hello"}"#,
                r#"{"type":"response.completed","response":{}}"#,
            ],
        )
        .unwrap();
        assert_eq!(
            events
                .iter()
                .filter_map(|event| match event {
                    ModelEvent::TextDelta(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["Hel", "lo"]
        );
    }

    #[test]
    fn maps_function_call_and_incomplete() {
        let events = collect_events(
            &mut Parser::new(),
            &[
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call-1","name":"lookup"}}"#,
                r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"q\":"}"#,
                r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"q\":\"a\"}"}"#,
                r#"{"type":"response.incomplete"}"#,
            ],
        )
        .unwrap();
        assert_eq!(events.last(), Some(&ModelEvent::Done(FinishReason::Length)));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallStart { call_id, name, .. } if call_id == "call-1" && name == "lookup"
        )));
    }

    #[test]
    fn mismatched_final_arguments_fail() {
        let error = collect_events(
            &mut Parser::new(),
            &[
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call-1","name":"lookup"}}"#,
                r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"q\":"}"#,
                r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"x\":1}"}"#,
            ],
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "OpenAI Responses final tool arguments do not match streamed arguments"
        );
    }

    #[test]
    fn missing_terminal_event_fails() {
        let error = collect_events(
            &mut Parser::new(),
            &[r#"{"type":"response.output_text.delta","delta":"Hi"}"#],
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "OpenAI Responses stream ended without response.completed or response.incomplete"
        );
    }

    #[test]
    fn saw_completed_item_fallback() {
        let events = collect_events(
            &mut Parser::new(),
            &[
                r#"{"type":"response.output_text.delta","delta":"Hi"}"#,
                r#"{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"Hi"}]}}"#,
            ],
        )
        .unwrap();
        assert_eq!(events.last(), Some(&ModelEvent::Done(FinishReason::Stop)));
    }
}
