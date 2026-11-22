//! OpenAI Chat Completions request body and SSE → ModelEvent.

use serde_json::{Value, json};

use super::{
    CoreMessage, ProtocolParser, core_message, count_usize, data_url, object, parse_object, text,
    usage_event,
};
use crate::{
    model::{ContentPart, ModelInvocation, ModelLatency},
    plugins::{Error, Result},
    provider::{FinishReason, ModelEvent},
};

const REPLAY_KIND: &str = "openai_chat";

struct ToolState {
    call_id: String,
    name: String,
    arguments: String,
    emitted: usize,
    started: bool,
}

pub(super) struct Parser {
    text_open: bool,
    thinking_open: bool,
    reasoning: String,
    tools: std::collections::BTreeMap<usize, ToolState>,
    final_usage: Option<ModelEvent>,
    finish: Option<FinishReason>,
    saw_done_marker: bool,
}

impl Parser {
    pub(super) fn new() -> Self {
        Self {
            text_open: false,
            thinking_open: false,
            reasoning: String::new(),
            tools: std::collections::BTreeMap::new(),
            final_usage: None,
            finish: None,
            saw_done_marker: false,
        }
    }
}

impl ProtocolParser for Parser {
    fn push_data(&mut self, payload: &str, emit: &mut dyn FnMut(ModelEvent)) -> Result<bool> {
        if payload == "[DONE]" {
            self.saw_done_marker = true;
            return Ok(true);
        }
        let value = parse_object(payload, "OpenAI Chat")?;
        if let Some(error) = event_error(&value) {
            return Err(Error::Provider(format!("OpenAI Chat error: {error}")));
        }
        if !value.get("usage").is_none_or(Value::is_null) {
            self.final_usage = Some(chat_usage(value.get("usage").unwrap()));
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
            text(delta.get("reasoning_content").unwrap_or(&Value::Null))
                .or_else(|| text(delta.get("reasoning").unwrap_or(&Value::Null)))
        });
        if let Some(reasoning_delta) = reasoning_delta {
            if !self.thinking_open {
                self.thinking_open = true;
                emit(ModelEvent::ThinkingStart);
            }
            self.reasoning.push_str(reasoning_delta);
            emit(ModelEvent::ThinkingDelta(reasoning_delta.to_owned()));
        }
        if let Some(content) = delta.and_then(|delta| text(delta.get("content").unwrap_or(&Value::Null)))
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
        if let Some(tool_calls) = delta.and_then(|delta| delta.get("tool_calls").and_then(Value::as_array))
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
                for event in update_tool(
                    index,
                    tool_delta.get("id").and_then(text),
                    function.and_then(|function| text(function.get("name").unwrap_or(&Value::Null))),
                    function
                        .and_then(|function| text(function.get("arguments").unwrap_or(&Value::Null))),
                    &mut self.tools,
                ) {
                    emit(event);
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(text) {
            self.finish = Some(map_finish(reason, !self.tools.is_empty()));
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
            if !tool.started {
                if tool.name.is_empty() {
                    return Err(Error::Provider(
                        "OpenAI Chat tool call is missing name".into(),
                    ));
                }
                if tool.call_id.is_empty() {
                    tool.call_id = format!("call-{index}");
                }
                tool.started = true;
                emit(ModelEvent::ToolCallStart {
                    index: *index,
                    call_id: tool.call_id.clone(),
                    name: tool.name.clone(),
                });
                if !tool.arguments.is_empty() {
                    tool.emitted = tool.arguments.len();
                    emit(ModelEvent::ToolCallArgumentsDelta {
                        index: *index,
                        delta: tool.arguments.clone(),
                    });
                }
            }
            emit(ModelEvent::ToolCallEnd { index: *index });
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
        let reason = self.finish.or_else(|| {
            self.saw_done_marker.then_some(if self.tools.is_empty() {
                FinishReason::Stop
            } else {
                FinishReason::ToolUse
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

pub(super) fn build_body(model: &str, invocation: &ModelInvocation) -> Result<Value> {
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
                let reasoning = replay_state.and_then(|state| {
                    (state.provider_kind == REPLAY_KIND)
                        .then(|| {
                            object(&state.value).and_then(|value| {
                                super::text(value.get("reasoning_content").unwrap_or(&Value::Null))
                            })
                        })
                        .flatten()
                });
                if text.is_empty() && tool_calls.is_empty() && reasoning.is_none() {
                    continue;
                }
                let mut value = json!({
                    "role": "assistant",
                    "content": if text.is_empty() { Value::Null } else { Value::String(text.into_owned()) },
                });
                if let Some(reasoning) = reasoning {
                    value
                        .as_object_mut()
                        .unwrap()
                        .insert("reasoning_content".into(), reasoning.into());
                }
                if !tool_calls.is_empty() {
                    value.as_object_mut().unwrap().insert(
                        "tool_calls".into(),
                        tool_calls
                            .iter()
                            .map(|tool_call| {
                                json!({
                                    "id": tool_call.call_id,
                                    "type": "function",
                                    "function": {
                                        "name": tool_call.name,
                                        "arguments": serde_json::to_string(&tool_call.arguments).unwrap_or_else(|_| "{}".into()),
                                    },
                                })
                            })
                            .collect::<Value>(),
                    );
                }
                messages.push(value);
            }
            CoreMessage::Tool(result) => {
                messages.push(json!({
                    "role": "tool",
                    "content": if result.provider_parts.is_empty() {
                        Value::String(result.content.clone())
                    } else {
                        chat_content(&result.provider_parts)?
                    },
                    "tool_call_id": result.call_id,
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
    let object = body.as_object_mut().unwrap();
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
                            "name": tool.name,
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
    if let Some(effort) = &request.model.reasoning.effort {
        object.insert("reasoning_effort".into(), effort.clone().into());
    }
    if request.model.latency == ModelLatency::Fast {
        object.insert("service_tier".into(), "fast".into());
    }
    object.insert(
        "prompt_cache_key".into(),
        invocation.conversation_id.clone().into(),
    );
    Ok(body)
}

fn chat_content(parts: &[ContentPart]) -> Result<Value> {
    if parts.iter().all(|part| matches!(part, ContentPart::Text { .. })) {
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
                Ok(json!({
                    "type": "file",
                    "file": {
                        "filename": filename.clone().unwrap_or_else(|| "attachment.pdf".into()),
                        "file_data": data_url(mime_type, data),
                    },
                }))
            }
        })
        .collect()
}

fn merge_fragment(target: &str, fragment: &str) -> String {
    if target == fragment || target.ends_with(fragment) {
        target.to_owned()
    } else if fragment.starts_with(target) {
        fragment.to_owned()
    } else {
        format!("{target}{fragment}")
    }
}

fn update_tool(
    index: usize,
    call_id: Option<&str>,
    name: Option<&str>,
    arguments_delta: Option<&str>,
    tools: &mut std::collections::BTreeMap<usize, ToolState>,
) -> Vec<ModelEvent> {
    let tool = tools.entry(index).or_insert_with(|| ToolState {
        call_id: String::new(),
        name: String::new(),
        arguments: String::new(),
        emitted: 0,
        started: false,
    });
    if let Some(call_id) = call_id {
        tool.call_id = merge_fragment(&tool.call_id, call_id);
    }
    if let Some(name) = name {
        tool.name = merge_fragment(&tool.name, name);
    }
    if let Some(delta) = arguments_delta {
        tool.arguments.push_str(delta);
    }
    let mut events = Vec::new();
    if !tool.started && !tool.call_id.is_empty() && !tool.name.is_empty() {
        tool.started = true;
        events.push(ModelEvent::ToolCallStart {
            index,
            call_id: tool.call_id.clone(),
            name: tool.name.clone(),
        });
    }
    if tool.started && tool.emitted < tool.arguments.len() {
        events.push(ModelEvent::ToolCallArgumentsDelta {
            index,
            delta: tool.arguments[tool.emitted..].to_owned(),
        });
        tool.emitted = tool.arguments.len();
    }
    events
}

fn event_error(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    if error.is_null() {
        return None;
    }
    if let Some(text) = error.as_str() {
        return Some(text.to_owned());
    }
    object(error)
        .and_then(|error| text(error.get("message").unwrap_or(&Value::Null)).map(str::to_owned))
        .or_else(|| Some(error.to_string()))
}

fn chat_usage(value: &Value) -> ModelEvent {
    let usage = object(value).cloned().unwrap_or_default();
    let details = usage.get("prompt_tokens_details").and_then(object);
    let completion = usage.get("completion_tokens_details").and_then(object);
    usage_event(
        usage.get("prompt_tokens").and_then(count_u64_value),
        usage.get("completion_tokens").and_then(count_u64_value),
        usage.get("total_tokens").and_then(count_u64_value),
        details.and_then(|details| details.get("cached_tokens").and_then(count_u64_value)),
        None,
        completion.and_then(|details| details.get("reasoning_tokens").and_then(count_u64_value)),
    )
}

fn count_u64_value(value: &Value) -> Option<u64> {
    super::count_u64(value)
}

fn map_finish(reason: &str, has_tools: bool) -> FinishReason {
    match reason {
        "tool_calls" | "function_call" => FinishReason::ToolUse,
        "length" => FinishReason::Length,
        "stop" | "content_filter" => FinishReason::Stop,
        _ => {
            if has_tools {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{
            ContentPart, ModelLatency, ProjectedContent, ProjectedMessage, ProviderReplayState,
            Role, ToolCallContent, ToolDefinition,
        },
        plugins::adapters::{collect_events, tests::invocation},
    };

    #[test]
    fn builds_chat_body_with_usage_and_fast_tier() {
        let mut invocation = invocation();
        invocation.request.model.max_output_tokens = Some(128);
        invocation.request.model.reasoning.effort = Some("low".into());
        invocation.request.model.latency = ModelLatency::Fast;
        invocation.request.prompt.tools = vec![ToolDefinition {
            name: "lookup".into(),
            description: "Look up".into(),
            parameters: json!({ "type": "object" }),
        }];
        let body = build_body("gpt-4.1", &invocation).unwrap();
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
        let body = build_body("gpt-4.1", &invocation).unwrap();
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
        let error = build_body("gpt-4.1", &invocation).unwrap_err();
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
        let body = build_body("gpt-4.1", &invocation).unwrap();
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
        let body = build_body("gpt-4.1", &invocation).unwrap();
        assert_eq!(body["messages"][0]["content"], Value::Null);
        assert_eq!(body["messages"][0]["reasoning_content"], "why");
        assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call-1");
    }

    #[test]
    fn parses_thinking_text_tools_usage_and_done() {
        let events = collect_events(
            &mut Parser::new(),
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
            &mut Parser::new(),
            &[r#"{"choices":[{"delta":{"content":"Hi"}}]}"#, "[DONE]"],
        )
        .unwrap();
        assert_eq!(events.last(), Some(&ModelEvent::Done(FinishReason::Stop)));
    }

    #[test]
    fn missing_done_and_finish_reason_fails() {
        let error = collect_events(
            &mut Parser::new(),
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
            &mut Parser::new(),
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
}
