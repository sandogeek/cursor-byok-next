//! `OpenAI` Chat Completions request body and SSE → `ModelEvent`.

use serde_json::{Value, json};

use super::{
    CoreMessage, ProtocolParser, SawTool, ToolNames, core_message, count_usize, data_url,
    map_text_field, object, parse_object, text,
};
use crate::{
    model::{ContentPart, ModelInvocation, ModelLatency, Usage},
    plugins::{Error, Result},
    provider::{FinishReason, ModelEvent},
};

use super::tools::ToolCalls;

const REPLAY_KIND: &str = "openai_chat";

pub(super) struct Parser {
    text_open: bool,
    thinking_open: bool,
    reasoning: String,
    tools: ToolCalls,
    tool_names: ToolNames,
    final_usage: Option<ModelEvent>,
    finish: Option<FinishReason>,
    saw_done_marker: bool,
}

impl Parser {
    pub(super) fn new(tool_names: ToolNames) -> Self {
        Self {
            text_open: false,
            thinking_open: false,
            reasoning: String::new(),
            tools: ToolCalls::new(),
            tool_names,
            final_usage: None,
            finish: None,
            saw_done_marker: false,
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
            self.saw_done_marker = true;
            return Ok(true);
        }
        let value = parse_object(payload, "OpenAI Chat")?;
        if let Some(error) = event_error(&value) {
            return Err(Error::Provider(format!("OpenAI Chat error: {error}")));
        }
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            self.final_usage = Some(chat_usage(usage));
        }
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(object)
        else {
            return Ok(false);
        };
        let delta = choice.get("delta").and_then(object);
        let reasoning_delta = delta.and_then(|delta| {
            delta
                .get("reasoning_content")
                .and_then(Value::as_str)
                .or_else(|| delta.get("reasoning").and_then(Value::as_str))
        });
        if let Some(reasoning_delta) = reasoning_delta {
            if !self.thinking_open {
                self.thinking_open = true;
                emit(ModelEvent::ThinkingStart);
            }
            self.reasoning.push_str(reasoning_delta);
            emit(ModelEvent::ThinkingDelta(reasoning_delta.to_owned()));
        }
        if let Some(content) = delta.and_then(|delta| delta.get("content").and_then(Value::as_str))
        {
            if self.thinking_open {
                self.thinking_open = false;
                emit(ModelEvent::ThinkingEnd);
            }
            if !self.text_open {
                self.text_open = true;
                emit(ModelEvent::TextStart);
            }
            emit(ModelEvent::TextDelta(content.to_owned()));
        }
        if let Some(tool_calls) =
            delta.and_then(|delta| delta.get("tool_calls").and_then(Value::as_array))
        {
            for (position, raw_tool) in tool_calls.iter().enumerate() {
                let Some(tool_delta) = object(raw_tool) else {
                    continue;
                };
                let index = tool_delta
                    .get("index")
                    .and_then(count_usize)
                    .unwrap_or(position);
                let function = tool_delta.get("function").and_then(object);
                let tool = self.tools.entry(index).or_default();
                if let Some(call_id) = tool_delta.get("id").and_then(text) {
                    tool.merge_call_id(call_id);
                }
                if let Some(name) =
                    function.and_then(|function| map_text_field(Some(function), "name"))
                {
                    tool.merge_name(name);
                }
                if let Some(delta) =
                    function.and_then(|function| map_text_field(Some(function), "arguments"))
                {
                    tool.push_arguments(delta);
                }
                for event in [
                    tool.take_start(index, &self.tool_names),
                    tool.take_arguments_delta(index),
                ]
                .into_iter()
                .flatten()
                {
                    emit(event);
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(text) {
            self.finish = Some(map_finish(reason, SawTool::from_count(self.tools.len())));
        }
        Ok(false)
    }

    fn finish(&mut self, emit: &mut dyn FnMut(ModelEvent)) -> Result<()> {
        if self.thinking_open {
            emit(ModelEvent::ThinkingEnd);
        }
        if self.text_open {
            emit(ModelEvent::TextEnd);
        }
        for (index, tool) in &mut self.tools {
            if !tool.is_started() {
                if tool.name_text().is_none() {
                    return Err(Error::Provider(
                        "OpenAI Chat tool call is missing name".into(),
                    ));
                }
                tool.default_call_id(*index);
                if let Some(event) = tool.take_start(*index, &self.tool_names) {
                    emit(event);
                }
                if let Some(event) = tool.take_arguments_delta(*index) {
                    emit(event);
                }
            }
            if let Some(event) = tool.take_end(*index) {
                emit(event);
            }
        }
        if let Some(usage) = self.final_usage.take() {
            emit(usage);
        }
        if !self.reasoning.is_empty() {
            emit(ModelEvent::ProviderReplayState(
                crate::model::ProviderReplayState {
                    provider_kind: REPLAY_KIND.into(),
                    value: json!({ "reasoning_content": self.reasoning }),
                },
            ));
        }
        let observed = SawTool::from_count(self.tools.len());
        let reason = self.finish.or_else(|| {
            self.saw_done_marker.then_some(match observed {
                SawTool::Absent => FinishReason::Stop,
                SawTool::Present => FinishReason::ToolUse,
            })
        });
        let Some(reason) = reason else {
            return Err(Error::Provider(
                "OpenAI Chat stream ended without finish_reason".into(),
            ));
        };
        emit(ModelEvent::Done(reason));
        Ok(())
    }
}

pub(super) fn build_body(
    model: &str,
    invocation: &ModelInvocation,
    tool_names: &ToolNames,
) -> Result<Value> {
    let request = &invocation.request;
    let mut messages = Vec::new();
    if !request.prompt.instructions.is_empty() {
        messages.push(json!({ "role": "system", "content": request.prompt.instructions }));
    }
    for message in &request.history {
        match core_message(message)? {
            CoreMessage::Assistant {
                text,
                replay_state,
                tool_calls,
            } => {
                if let Some(value) = assistant_message(text, replay_state, tool_calls, tool_names)?
                {
                    messages.push(value);
                }
            }
            CoreMessage::Tool(result) => {
                messages.push(json!({
                    "role": "tool",
                    "content": if result.provider_parts.is_empty() {
                        Value::String(result.content.clone())
                    } else {
                        chat_content(&result.provider_parts)?
                    },
                    "tool_call_id": super::truncated_call_id(&result.call_id),
                }));
            }
            CoreMessage::System(content) => {
                messages.push(json!({
                    "role": "system",
                    "content": chat_content(content)?,
                }));
            }
            CoreMessage::User(content) => {
                messages.push(json!({
                    "role": "user",
                    "content": chat_content(content)?,
                }));
            }
        }
    }
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    let object = super::object_mut(&mut body)?;
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
                        "function": {
                            "name": tool_names.alias(&tool.name),
                            "description": tool.description,
                            "parameters": tool.parameters,
                        },
                    })
                })
                .collect::<Value>(),
        );
    }
    if let Some(tokens) = request.model.max_output_tokens {
        object.insert("max_completion_tokens".into(), tokens.into());
    }
    if let Some(effort) = request.model.reasoning.effort() {
        object.insert("reasoning_effort".into(), effort.into());
    }
    if request.model.latency == ModelLatency::Fast {
        object.insert("service_tier".into(), "fast".into());
    }
    super::apply_openai_prompt_cache_key(&mut body, model, &invocation.conversation_id);
    super::merge_extra_params(&mut body, &request.model.extra_params)?;
    super::apply_stop_sequences(&mut body, "stop", &request.model.stop_sequences)?;
    Ok(body)
}

/// Renders one assistant turn, or `None` when the turn carries nothing to replay.
fn assistant_message(
    text: std::borrow::Cow<'_, str>,
    replay_state: Option<&crate::model::ProviderReplayState>,
    tool_calls: &[crate::model::ToolCallContent],
    tool_names: &ToolNames,
) -> crate::plugins::Result<Option<Value>> {
    let reasoning = replay_state
        .filter(|state| state.provider_kind == REPLAY_KIND)
        .and_then(|state| object(&state.value))
        .and_then(|value| value.get("reasoning_content").and_then(Value::as_str));
    if text.is_empty() && tool_calls.is_empty() && reasoning.is_none() {
        return Ok(None);
    }
    let mut message = serde_json::Map::new();
    message.insert("role".into(), Value::from("assistant"));
    message.insert(
        "content".into(),
        if text.is_empty() {
            Value::Null
        } else {
            Value::String(text.into_owned())
        },
    );
    if let Some(reasoning) = reasoning {
        message.insert("reasoning_content".into(), reasoning.into());
    }
    if !tool_calls.is_empty() {
        let mut projected = Vec::with_capacity(tool_calls.len());
        for tool_call in tool_calls {
            projected.push(json!({
                "id": super::truncated_call_id(&tool_call.call_id),
                "type": "function",
                "function": {
                    "name": tool_names.alias(&tool_call.name),
                    "arguments": serde_json::to_string(&tool_call.arguments)?,
                },
            }));
        }
        message.insert("tool_calls".into(), Value::Array(projected));
    }
    Ok(Some(Value::Object(message)))
}

fn chat_content(parts: &[ContentPart]) -> Result<Value> {
    if parts
        .iter()
        .all(|part| matches!(part, ContentPart::Text { .. }))
    {
        return Ok(Value::String(
            parts
                .iter()
                .map(|part| match part {
                    ContentPart::Text { text } => text.as_str(),
                    _ => "",
                })
                .collect(),
        ));
    }
    parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => Ok(json!({ "type": "text", "text": text })),
            ContentPart::Image { mime_type, data } => Ok(json!({
                "type": "image_url",
                "image_url": { "url": data_url(mime_type, data) },
            })),
            ContentPart::File {
                mime_type,
                filename,
                data,
            } => {
                if mime_type != "application/pdf" {
                    return Err(Error::Provider(
                        "OpenAI Chat Completions only supports PDF file attachments".into(),
                    ));
                }
                let filename = filename.as_deref().unwrap_or("attachment.pdf");
                Ok(json!({
                    "type": "file",
                    "file": {
                        "filename": filename,
                        "file_data": data_url(mime_type, data),
                    },
                }))
            }
        })
        .collect()
}

/// `OpenAI` Chat only carries the message; the error class adds nothing.
fn event_error(value: &Value) -> Option<String> {
    super::event_error_detail(value).map(|(_, message)| message)
}

fn chat_usage(value: &Value) -> ModelEvent {
    let usage = object(value);
    let details = usage
        .and_then(|usage| usage.get("prompt_tokens_details"))
        .and_then(object);
    let completion = usage
        .and_then(|usage| usage.get("completion_tokens_details"))
        .and_then(object);
    ModelEvent::Usage(
        Usage::new(
            super::map_count(usage, "prompt_tokens"),
            super::map_count(usage, "completion_tokens"),
            super::map_count(usage, "total_tokens"),
        )
        .with_cache_read(super::map_count(details, "cached_tokens"))
        .with_reasoning(super::map_count(completion, "reasoning_tokens")),
    )
}

fn map_finish(reason: &str, has_tools: SawTool) -> FinishReason {
    match reason {
        "tool_calls" | "function_call" => FinishReason::ToolUse,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::Stop,
        _ if has_tools == SawTool::Present => FinishReason::ToolUse,
        _ => FinishReason::Stop,
    }
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
    fn builds_chat_body_with_usage_and_fast_tier() {
        let mut invocation = invocation();
        invocation.request.model.max_output_tokens = Some(128);
        invocation.request.model.reasoning = Reasoning::Effort("low".into());
        invocation.request.model.latency = ModelLatency::Fast;
        invocation.request.prompt.tools = vec![ToolDefinition {
            name: "lookup".into(),
            description: "Look up".into(),
            parameters: json!({ "type": "object" }),
        }];
        let body = build_body("gpt-4.1", &invocation, &names(&invocation)).unwrap();
        assert_eq!(body["model"], "gpt-4.1");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["max_completion_tokens"], 128);
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["service_tier"], "fast");
        assert_eq!(body["prompt_cache_key"], "conversation-1");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "You are concise.");
        assert_eq!(body["messages"][1]["content"], "hello");
        assert_eq!(body["tools"][0]["function"]["name"], "lookup");
    }

    #[test]
    fn flattens_text_and_expands_images() {
        let mut invocation = invocation();
        invocation.request.prompt.instructions.clear();
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
        let body = build_body("gpt-4.1", &invocation, &names(&invocation)).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(
            body["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AAECAw=="
        );
    }

    #[test]
    fn rejects_non_pdf_files() {
        let mut invocation = invocation();
        invocation.request.history = vec![ProjectedMessage {
            message_id: "m1".into(),
            role: Role::User,
            content: ProjectedContent::Parts(vec![ContentPart::File {
                mime_type: "text/plain".into(),
                filename: Some("notes.txt".into()),
                data: b"f".to_vec(),
            }]),
        }];
        let error = build_body("gpt-4.1", &invocation, &names(&invocation)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "OpenAI Chat Completions only supports PDF file attachments"
        );
    }

    #[test]
    fn skips_empty_assistant_without_tools_or_reasoning() {
        let mut invocation = invocation();
        invocation.request.prompt.instructions.clear();
        invocation.request.history = vec![
            ProjectedMessage {
                message_id: "m1".into(),
                role: Role::Assistant,
                content: ProjectedContent::Assistant {
                    text: String::new(),
                    thinking: String::new(),
                    replay_state: None,
                    calls: Vec::new(),
                },
            },
            ProjectedMessage {
                message_id: "m2".into(),
                role: Role::User,
                content: ProjectedContent::Parts(vec![ContentPart::Text { text: "hi".into() }]),
            },
        ];
        let body = build_body("gpt-4.1", &invocation, &names(&invocation)).unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn replays_reasoning_and_tool_calls() {
        let mut invocation = invocation();
        invocation.request.prompt.instructions.clear();
        invocation.request.history = vec![ProjectedMessage {
            message_id: "m1".into(),
            role: Role::Assistant,
            content: ProjectedContent::Assistant {
                text: String::new(),
                thinking: String::new(),
                replay_state: Some(ProviderReplayState {
                    provider_kind: REPLAY_KIND.into(),
                    value: json!({ "reasoning_content": "why" }),
                }),
                calls: vec![ToolCallContent {
                    index: 0,
                    call_id: "call-1".into(),
                    name: "lookup".into(),
                    arguments: json!({ "q": "a" }),
                }],
            },
        }];
        let body = build_body("gpt-4.1", &invocation, &names(&invocation)).unwrap();
        assert_eq!(body["messages"][0]["content"], Value::Null);
        assert_eq!(body["messages"][0]["reasoning_content"], "why");
        assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call-1");
    }

    #[test]
    fn parses_thinking_text_tools_usage_and_done() {
        let events = collect_events(
            &mut Parser::new(ToolNames::default()),
            &[
                r#"{"choices":[{"delta":{"reasoning_content":"why"}}]}"#,
                r#"{"choices":[{"delta":{"content":"Hi"}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"lookup","arguments":"{\"q\":"}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a\"}"}}]},"finish_reason":"tool_calls"}]}"#,
                r#"{"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5,"prompt_tokens_details":{"cached_tokens":1},"completion_tokens_details":{"reasoning_tokens":4}}}"#,
                "[DONE]",
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
                ModelEvent::ToolCallStart {
                    index: 0,
                    call_id: "call-1".into(),
                    name: "lookup".into(),
                },
                ModelEvent::ToolCallArgumentsDelta {
                    index: 0,
                    delta: "{\"q\":".into(),
                },
                ModelEvent::ToolCallArgumentsDelta {
                    index: 0,
                    delta: "\"a\"}".into(),
                },
                ModelEvent::TextEnd,
                ModelEvent::ToolCallEnd { index: 0 },
                ModelEvent::Usage(crate::model::Usage {
                    input_tokens: Some(3),
                    context_input_tokens: Some(3),
                    output_tokens: Some(2),
                    total_tokens: Some(5),
                    cache_read_tokens: Some(1),
                    cache_write_tokens: None,
                    reasoning_tokens: Some(4),
                }),
                ModelEvent::ProviderReplayState(crate::model::ProviderReplayState {
                    provider_kind: REPLAY_KIND.into(),
                    value: json!({ "reasoning_content": "why" }),
                }),
                ModelEvent::Done(FinishReason::ToolUse),
            ]
        );
    }

    #[test]
    fn done_without_finish_reason_infers_stop() {
        let events = collect_events(
            &mut Parser::new(ToolNames::default()),
            &[r#"{"choices":[{"delta":{"content":"Hi"}}]}"#, "[DONE]"],
        )
        .unwrap();
        assert_eq!(events.last(), Some(&ModelEvent::Done(FinishReason::Stop)));
    }

    #[test]
    fn missing_done_and_finish_reason_fails() {
        let error = collect_events(
            &mut Parser::new(ToolNames::default()),
            &[r#"{"choices":[{"delta":{"content":"Hi"}}]}"#],
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "OpenAI Chat stream ended without finish_reason"
        );
    }

    #[test]
    fn merges_repeated_tool_id_fragments() {
        let events = collect_events(
            &mut Parser::new(ToolNames::default()),
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call"}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"n","arguments":"{}"}}]},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ],
        )
        .unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallStart { call_id, .. } if call_id == "call-1"
        )));
    }

    #[test]
    fn observed_tool_calls_outrank_ordinary_stop_reasons() {
        assert_eq!(map_finish("stop", SawTool::Present), FinishReason::ToolUse);
        assert_eq!(map_finish("", SawTool::Present), FinishReason::ToolUse);
        assert_eq!(
            map_finish("content_filter", SawTool::Present),
            FinishReason::Stop
        );
        assert_eq!(map_finish("length", SawTool::Present), FinishReason::Length);
    }

    #[test]
    fn typed_error_events_fail_the_stream() {
        let error = collect_events(
            &mut Parser::new(ToolNames::default()),
            &[r#"{"type":"error","message":"quota exceeded"}"#],
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "OpenAI Chat error: quota exceeded");
    }

    #[test]
    fn omits_prompt_cache_key_for_non_gpt_models() {
        let body = build_body("claude-sonnet", &invocation(), &names(&invocation())).unwrap();
        assert!(body.get("prompt_cache_key").is_none());
    }
}
