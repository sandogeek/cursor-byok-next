//! `OpenAI` Responses request body and SSE → `ModelEvent`.

use serde_json::{Value, json};

use super::tools::{ToolCalls, ToolState};
use super::{
    CoreMessage, ProtocolParser, SawTool, ToolNames, core_message, count_usize, data_url,
    map_text_field, object, object_mut, parse_object, text_field,
};
use crate::{
    model::{ContentPart, ModelInvocation, ModelLatency, Usage},
    plugins::{Error, Result},
    provider::{FinishReason, ModelEvent},
};

const REPLAY_KIND: &str = "openai_responses";

pub(super) struct Parser {
    text_open: bool,
    streamed_text: String,
    thinking_open: bool,
    tools: ToolCalls,
    tool_names: ToolNames,
    reasoning_items: Vec<Value>,
    saw_tool: SawTool,
    completion: Completion,
}

/// How far the terminal handshake has advanced. Two separate `bool`s would let
/// `terminal` be true while `saw_completed_item` stayed false.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Completion {
    /// No complete output item yet, and no terminal event.
    Pending,
    /// At least one complete output item arrived.
    ItemsCompleted,
    /// `response.completed` or `response.incomplete` arrived.
    Terminal,
}

impl Completion {
    fn is_terminal(self) -> bool {
        self == Self::Terminal
    }
}

impl Parser {
    pub(super) fn new(tool_names: ToolNames) -> Self {
        Self {
            text_open: false,
            streamed_text: String::new(),
            thinking_open: false,
            tools: ToolCalls::new(),
            tool_names,
            reasoning_items: Vec::new(),
            saw_tool: SawTool::Absent,
            completion: Completion::Pending,
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
        if final_text.starts_with(&self.streamed_text)
            && final_text.len() > self.streamed_text.len()
        {
            if !self.text_open {
                self.text_open = true;
                emit(ModelEvent::TextStart);
            }
            emit(ModelEvent::TextDelta(
                final_text[self.streamed_text.len()..].to_owned(),
            ));
            final_text.clone_into(&mut self.streamed_text);
        }
    }

    fn end_started_tools(&mut self, emit: &mut dyn FnMut(ModelEvent)) {
        for (index, tool) in &mut self.tools {
            if tool.is_started()
                && let Some(event) = tool.take_end(*index)
            {
                emit(event);
            }
        }
    }

    fn emit_replay_state(&mut self, emit: &mut dyn FnMut(ModelEvent)) {
        if !self.reasoning_items.is_empty() {
            emit(ModelEvent::ProviderReplayState(
                crate::model::ProviderReplayState {
                    provider_kind: REPLAY_KIND.into(),
                    value: json!({ "items": std::mem::take(&mut self.reasoning_items) }),
                },
            ));
        }
    }

    fn output_item_added(&mut self, value: &Value, emit: &mut dyn FnMut(ModelEvent)) -> Result<()> {
        let item = value.get("item").and_then(object);
        if map_text_field(item, "type") != Some("function_call") {
            return Ok(());
        }
        self.saw_tool = SawTool::Present;
        let index = required_index(value)?;
        let identity = item_identity(item);
        self.tool(index).fill_identity(identity.0, identity.1);
        self.emit_pending_tool(index, emit);
        Ok(())
    }

    fn output_item_done(&mut self, value: &Value, emit: &mut dyn FnMut(ModelEvent)) -> Result<()> {
        let item = value.get("item").and_then(object);
        match map_text_field(item, "type") {
            Some("reasoning") => {
                self.close_thinking(emit);
                self.reasoning_items
                    .push(value.get("item").cloned().unwrap_or(Value::Null));
            }
            Some("message") => {
                self.completion = Completion::ItemsCompleted;
                if let Some(item) = item
                    && let Some(final_text) = item_text(item)
                {
                    self.reconcile_text(&final_text, emit);
                }
                self.close_text(emit);
            }
            Some("function_call") => {
                self.completion = Completion::ItemsCompleted;
                self.saw_tool = SawTool::Present;
                let index = required_index(value)?;
                let tool = self.tool(index);
                let identity = item_identity(item);
                tool.fill_identity(identity.0, identity.1);
                if let Some(snapshot) = map_text_field(item, "arguments") {
                    tool.extend_to_snapshot(snapshot)?;
                }
                self.emit_pending_tool(index, emit);
                if let Some(event) = self.tool(index).take_end(index) {
                    emit(event);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn function_call_arguments_delta(
        &mut self,
        value: &Value,
        emit: &mut dyn FnMut(ModelEvent),
    ) -> Result<()> {
        let Some(delta) = text_field(value, "delta") else {
            return Ok(());
        };
        self.saw_tool = SawTool::Present;
        let index = required_index(value)?;
        self.tool(index).push_arguments(delta);
        self.emit_pending_tool(index, emit);
        Ok(())
    }

    fn function_call_arguments_done(
        &mut self,
        value: &Value,
        emit: &mut dyn FnMut(ModelEvent),
    ) -> Result<()> {
        let snapshot = text_field(value, "arguments");
        let index = required_index(value)?;
        if let Some(snapshot) = snapshot.filter(|snapshot| !snapshot.is_empty()) {
            self.tool(index).extend_to_snapshot(snapshot)?;
        }
        self.emit_pending_tool(index, emit);
        if snapshot.is_some_and(|snapshot| !snapshot.is_empty())
            && let Some(event) = self.tool(index).take_end(index)
        {
            emit(event);
        }
        Ok(())
    }

    fn tool(&mut self, index: usize) -> &mut ToolState {
        self.tools.entry(index).or_default()
    }

    /// Sends what this call still owes: its start, then its argument delta.
    fn emit_pending_tool(&mut self, index: usize, emit: &mut dyn FnMut(ModelEvent)) {
        let tool = self.tools.entry(index).or_default();
        let start = tool.take_start(index, &self.tool_names);
        let delta = tool.take_arguments_delta(index);
        for event in [start, delta].into_iter().flatten() {
            emit(event);
        }
    }
}

impl ProtocolParser for Parser {
    fn push_data(
        &mut self,
        payload: &str,
        _event: &str,
        emit: &mut dyn FnMut(ModelEvent),
    ) -> Result<bool> {
        if payload == "[DONE]" {
            return Ok(true);
        }
        let value = parse_object(payload, "OpenAI Responses")?;
        match text_field(&value, "type") {
            Some("response.output_text.delta") => {
                self.close_thinking(emit);
                if !self.text_open {
                    self.text_open = true;
                    emit(ModelEvent::TextStart);
                }
                if let Some(delta) = text_field(&value, "delta") {
                    self.streamed_text.push_str(delta);
                    emit(ModelEvent::TextDelta(delta.to_owned()));
                }
            }
            Some("response.output_text.done") => {
                if let Some(final_text) = text_field(&value, "text") {
                    self.reconcile_text(final_text, emit);
                }
                self.close_text(emit);
            }
            Some("response.reasoning_summary_text.delta" | "response.reasoning_text.delta") => {
                if !self.thinking_open {
                    self.thinking_open = true;
                    emit(ModelEvent::ThinkingStart);
                }
                if let Some(delta) = text_field(&value, "delta") {
                    emit(ModelEvent::ThinkingDelta(delta.to_owned()));
                }
            }
            Some("response.reasoning_summary_text.done" | "response.reasoning_text.done") => {
                self.close_thinking(emit);
            }
            Some("response.output_item.added") => self.output_item_added(&value, emit)?,
            Some("response.output_item.done") => self.output_item_done(&value, emit)?,
            Some("response.function_call_arguments.delta") => {
                self.function_call_arguments_delta(&value, emit)?;
            }
            Some("response.function_call_arguments.done") => {
                self.function_call_arguments_done(&value, emit)?;
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
                if self.tools.values().any(|tool| !tool.is_started()) {
                    return Err(Error::Provider(
                        "OpenAI Responses completed with incomplete tool metadata".into(),
                    ));
                }
                self.completion = Completion::Terminal;
                self.emit_replay_state(emit);
                emit(ModelEvent::Done(finish_reason(self.saw_tool)));
                return Ok(true);
            }
            Some("response.incomplete") => {
                self.close_thinking(emit);
                self.close_text(emit);
                self.end_started_tools(emit);
                self.completion = Completion::Terminal;
                emit(ModelEvent::Done(FinishReason::Length));
                return Ok(true);
            }
            Some("response.failed") => {
                return Err(Error::Provider(format!(
                    "OpenAI Responses failed: {payload}"
                )));
            }
            Some("error") => {
                return Err(Error::Provider(format!(
                    "OpenAI Responses error: {payload}"
                )));
            }
            _ => {}
        }
        Ok(false)
    }

    fn finish(&mut self, emit: &mut dyn FnMut(ModelEvent)) -> Result<()> {
        if self.completion == Completion::ItemsCompleted {
            self.close_thinking(emit);
            self.close_text(emit);
            if self.tools.values().any(|tool| !tool.is_ended()) {
                return Err(Error::Provider(
                    "OpenAI Responses stream ended with an incomplete tool call".into(),
                ));
            }
            self.completion = Completion::Terminal;
            self.emit_replay_state(emit);
            emit(ModelEvent::Done(finish_reason(self.saw_tool)));
        }
        if !self.completion.is_terminal() {
            return Err(Error::Provider(
                "OpenAI Responses stream ended without response.completed or response.incomplete"
                    .into(),
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
    let input = responses_input(&request.history, tool_names)?;
    let mut body = json!({
        "model": model,
        "input": input,
        "stream": true,
        "instructions": request.prompt.instructions,
        "include": ["reasoning.encrypted_content"],
    });
    apply_responses_options(&mut body, request, tool_names)?;
    super::apply_openai_prompt_cache_key(&mut body, model, &invocation.conversation_id);
    // OpenAI Responses has no stop-sequence field; a `stop` key would be rejected
    // by strict gateways, so Cursor's stop sequences are dropped for this protocol.
    super::merge_extra_params(&mut body, &request.model.extra_params)?;
    Ok(body)
}

/// Projects the conversation into Responses `input` items.
fn responses_input(
    history: &[crate::model::ProjectedMessage],
    tool_names: &ToolNames,
) -> Result<Vec<Value>> {
    let mut input = Vec::new();
    for message in history {
        match core_message(message)? {
            CoreMessage::Assistant {
                text,
                replay_state,
                tool_calls,
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
                        "content": [{ "type": "output_text", "text": text.as_ref() }],
                    }));
                }
                for tool_call in tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": super::truncated_call_id(&tool_call.call_id),
                        "name": tool_names.alias(&tool_call.name),
                        "arguments": serde_json::to_string(&tool_call.arguments)?,
                    }));
                }
            }
            CoreMessage::Tool(result) => {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": super::truncated_call_id(&result.call_id),
                    "output": if result.provider_parts.is_empty() {
                        Value::String(result.content.clone())
                    } else {
                        Value::Array(content_parts(&result.provider_parts))
                    },
                }));
            }
            CoreMessage::System(content) => {
                let parts = content_parts(content);
                if !parts.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "system",
                        "content": parts,
                    }));
                }
            }
            CoreMessage::User(content) => {
                let parts = content_parts(content);
                if !parts.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": parts,
                    }));
                }
            }
        }
    }
    Ok(input)
}

fn apply_responses_options(
    body: &mut Value,
    request: &crate::model::ModelRequest,
    tool_names: &ToolNames,
) -> Result<()> {
    let object = object_mut(body)?;
    if !request.prompt.tools.is_empty() {
        object.insert(
            "tools".into(),
            request
                .prompt
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool_names.alias(&tool.name),
                        "description": tool.description,
                        "parameters": tool.parameters,
                        "strict": false,
                    })
                })
                .collect::<Value>(),
        );
    }
    if let Some(tokens) = request.model.max_output_tokens {
        object.insert("max_output_tokens".into(), tokens.into());
    }
    if request.model.reasoning.is_enabled() {
        let mut reasoning = json!({ "summary": "auto" });
        if let Some(effort) = request.model.reasoning.effort()
            && let Some(reasoning) = reasoning.as_object_mut()
        {
            reasoning.insert("effort".into(), effort.into());
        }
        object.insert("reasoning".into(), reasoning);
    }
    if request.model.latency == ModelLatency::Fast {
        object.insert("service_tier".into(), "priority".into());
    }
    Ok(())
}

/// The finish reason for a completed Responses stream.
fn finish_reason(saw_tool: SawTool) -> FinishReason {
    match saw_tool {
        SawTool::Present => FinishReason::ToolUse,
        SawTool::Absent => FinishReason::Stop,
    }
}

fn content_parts(parts: &[ContentPart]) -> Vec<Value> {
    let mut content = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            ContentPart::Text { text } => {
                if !text.is_empty() {
                    content.push(json!({ "type": "input_text", "text": text }));
                }
            }
            ContentPart::Image { mime_type, data } => content.push(json!({
                "type": "input_image",
                "detail": "auto",
                "image_url": data_url(mime_type, data),
            })),
            ContentPart::File {
                mime_type,
                filename,
                data,
            } => content.push(json!({
                "type": "input_file",
                "filename": filename.as_deref().unwrap_or("attachment"),
                "file_data": data_url(mime_type, data),
            })),
        }
    }
    content
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
                Error::Provider(
                    "OpenAI Responses replay state contains a non-reasoning item".into(),
                )
            })?;
            if map_text_field(Some(source), "type") != Some("reasoning") {
                return Err(Error::Provider(
                    "OpenAI Responses replay state contains a non-reasoning item".into(),
                ));
            }
            let mut projected = json!({ "type": "reasoning" });
            for field in ["id", "summary", "content", "encrypted_content"] {
                if let Some(value) = source.get(field)
                    && let Some(projected) = projected.as_object_mut()
                {
                    projected.insert(field.into(), value.clone());
                }
            }
            Ok(projected)
        })
        .collect()
}

/// The `call_id` and `name` a Responses output item carries.
fn item_identity(item: Option<&serde_json::Map<String, Value>>) -> (Option<&str>, Option<&str>) {
    (
        map_text_field(item, "call_id"),
        map_text_field(item, "name"),
    )
}

fn item_text(item: &serde_json::Map<String, Value>) -> Option<String> {
    let content = item.get("content")?.as_array()?;
    Some(
        content
            .iter()
            .filter_map(object)
            .filter(|part| map_text_field(Some(part), "type") == Some("output_text"))
            .map(|part| map_text_field(Some(part), "text").unwrap_or(""))
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
    let usage = object(value);
    let input_details = usage
        .and_then(|usage| usage.get("input_tokens_details"))
        .and_then(object);
    let output_details = usage
        .and_then(|usage| usage.get("output_tokens_details"))
        .and_then(object);
    ModelEvent::Usage(
        Usage::new(
            super::map_count(usage, "input_tokens"),
            super::map_count(usage, "output_tokens"),
            super::map_count(usage, "total_tokens"),
        )
        .with_cache_read(super::map_count(input_details, "cached_tokens"))
        .with_reasoning(super::map_count(output_details, "reasoning_tokens")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{
            ContentPart, ModelLatency, ProjectedContent, ProjectedMessage, ProviderReplayState,
            Reasoning, Role, ToolCallContent, ToolDefinition,
        },
        plugins::adapters::{ToolNames, collect_events, tests::invocation},
    };

    fn names(invocation: &ModelInvocation) -> ToolNames {
        ToolNames::new(invocation)
    }

    #[test]
    fn builds_responses_body_with_priority_tier() {
        let mut invocation = invocation();
        invocation.request.model.latency = ModelLatency::Fast;
        invocation.request.model.reasoning = Reasoning::Effort("high".into());
        invocation.request.prompt.tools = vec![ToolDefinition {
            name: "lookup".into(),
            description: "Look up".into(),
            parameters: json!({ "type": "object" }),
        }];
        let body = build_body("gpt-5", &invocation, &names(&invocation)).unwrap();
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
        let mut invocation = invocation();
        invocation.request.prompt.instructions.clear();
        invocation.request.history = vec![ProjectedMessage {
            message_id: "m1".into(),
            role: Role::Assistant,
            content: ProjectedContent::Assistant {
                text: "ok".into(),
                thinking: String::new(),
                replay_state: Some(ProviderReplayState {
                    provider_kind: REPLAY_KIND.into(),
                    value: json!({
                        "items": [{
                            "type": "reasoning",
                            "id": "r1",
                            "encrypted_content": "enc"
                        }]
                    }),
                }),
                calls: vec![ToolCallContent {
                    index: 0,
                    call_id: "call-1".into(),
                    name: "lookup".into(),
                    arguments: json!({ "q": "a" }),
                }],
            },
        }];
        let body = build_body("gpt-5", &invocation, &names(&invocation)).unwrap();
        assert_eq!(body["input"][0]["type"], "reasoning");
        assert_eq!(body["input"][0]["encrypted_content"], "enc");
        assert_eq!(body["input"][1]["type"], "message");
        assert_eq!(body["input"][2]["type"], "function_call");
    }

    #[test]
    fn rejects_non_reasoning_replay() {
        let mut invocation = invocation();
        invocation.request.history = vec![ProjectedMessage {
            message_id: "m1".into(),
            role: Role::Assistant,
            content: ProjectedContent::Assistant {
                text: String::new(),
                thinking: String::new(),
                replay_state: Some(ProviderReplayState {
                    provider_kind: REPLAY_KIND.into(),
                    value: json!({ "items": [{ "type": "message" }] }),
                }),
                calls: Vec::new(),
            },
        }];
        let error = build_body("gpt-5", &invocation, &names(&invocation)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "OpenAI Responses replay state contains a non-reasoning item"
        );
    }

    #[test]
    fn serializes_files_and_images() {
        let mut invocation = invocation();
        invocation.request.prompt.instructions.clear();
        invocation.request.history = vec![ProjectedMessage {
            message_id: "m1".into(),
            role: Role::User,
            content: ProjectedContent::Parts(vec![
                ContentPart::Image {
                    mime_type: "image/png".into(),
                    data: vec![0],
                },
                ContentPart::File {
                    mime_type: "application/pdf".into(),
                    filename: Some("report.pdf".into()),
                    data: b"%PDF-1".to_vec(),
                },
            ]),
        }];
        let body = build_body("gpt-5", &invocation, &names(&invocation)).unwrap();
        assert_eq!(body["input"][0]["content"][0]["type"], "input_image");
        assert_eq!(body["input"][0]["content"][1]["type"], "input_file");
        assert_eq!(body["input"][0]["content"][1]["filename"], "report.pdf");
    }

    #[test]
    fn parses_text_thinking_usage_and_completed() {
        let events = collect_events(
            &mut Parser::new(ToolNames::default()),
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
            &mut Parser::new(ToolNames::default()),
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
            &mut Parser::new(ToolNames::default()),
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
            &mut Parser::new(ToolNames::default()),
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
            &mut Parser::new(ToolNames::default()),
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
            &mut Parser::new(ToolNames::default()),
            &[
                r#"{"type":"response.output_text.delta","delta":"Hi"}"#,
                r#"{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"Hi"}]}}"#,
            ],
        )
        .unwrap();
        assert_eq!(events.last(), Some(&ModelEvent::Done(FinishReason::Stop)));
    }

    #[test]
    fn omits_prompt_cache_key_for_non_gpt_models() {
        let body = build_body("claude-sonnet", &invocation(), &names(&invocation())).unwrap();
        assert!(body.get("prompt_cache_key").is_none());
    }
}
