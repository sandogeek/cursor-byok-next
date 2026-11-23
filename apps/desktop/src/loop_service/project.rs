//! Projects client-owned model calls into the provider-independent request.

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Map, Value};

use crate::{
    Error, Result,
    model::{
        ContentPart, ModelRequest, ModelSpec, ProjectedContent, ProjectedMessage, PromptSpec,
        ProviderReplayState, Role, ToolCallContent, ToolDefinition, ToolResultContent,
        parse_token_count,
    },
    plugins::PluginModelDescriptor,
    protocol::{
        json::{prost_json, prost_struct},
        proto::inference as pb,
    },
};

/// `InferenceCoreMessage.role` values, as Cursor sends them.
const ROLE_USER: i32 = 1;
const ROLE_ASSISTANT: i32 = 2;
const ROLE_TOOL: i32 = 3;
const ROLE_SYSTEM_MESSAGE: i32 = 4;

pub(crate) fn project(
    input: &pb::InferenceStreamRequest,
    model_id: &str,
    configured_context_window_tokens: Option<u64>,
    models: &[PluginModelDescriptor],
) -> Result<ModelRequest> {
    let model = project_model(input, model_id, configured_context_window_tokens)?;
    let (instructions, prefix_len) = project_instructions(&input.messages)?;
    Ok(ModelRequest {
        prompt: PromptSpec {
            instructions,
            tools: project_tools(input, models),
        },
        model,
        history: project_history(&input.messages[prefix_len..], prefix_len)?,
    })
}

/// Builds the model spec: context window, output limit, sampling params and
/// reasoning effort, all taken from the client's requested model.
fn project_model(
    input: &pb::InferenceStreamRequest,
    model_id: &str,
    configured_context_window_tokens: Option<u64>,
) -> Result<ModelSpec> {
    let mut model = ModelSpec::new(model_id);
    if let Some(config) = &input.model_config {
        model.max_output_tokens = config
            .max_tokens
            .and_then(|value| u64::try_from(value).ok());
        model.stop_sequences.clone_from(&config.stop_sequences);
        let mut extra = Map::new();
        if let Some(value) = config.temperature {
            extra.insert("temperature".into(), Value::from(value));
        }
        if let Some(value) = config.top_p {
            extra.insert("top_p".into(), Value::from(value));
        }
        model.extra_params = Value::Object(extra);
    }
    if let Some(requested) = &input.requested_model {
        model.reasoning.enabled = requested.max_mode;
        for parameter in &requested.parameters {
            match parameter.id.as_str() {
                "effort" | "reasoning" => {
                    model.reasoning.effort = Some(parameter.value.clone());
                    model.reasoning.enabled = true;
                }
                "context" => {
                    model.context_window_tokens = Some(parse_context_tokens(&parameter.value)?);
                }
                _ => {}
            }
        }
    }
    if model.context_window_tokens.is_none() {
        model.context_window_tokens = configured_context_window_tokens;
    }
    Ok(model)
}

fn project_tools(
    input: &pb::InferenceStreamRequest,
    models: &[PluginModelDescriptor],
) -> Vec<ToolDefinition> {
    input
        .tools
        .iter()
        .map(|tool| {
            let parameters = tool
                .parameters
                .as_ref()
                .map_or_else(|| serde_json::json!({}), prost_struct);
            ToolDefinition {
                name: tool.name.clone(),
                description: subagent_models_task_description(
                    &tool.name,
                    &tool.description,
                    models,
                ),
                parameters: parameters.get("jsonSchema").cloned().unwrap_or(parameters),
            }
        })
        .collect()
}

/// Concatenates the leading system messages into the prompt instructions and
/// reports how many messages they consumed.
fn project_instructions(messages: &[pb::InferenceCoreMessage]) -> Result<(String, usize)> {
    let mut instructions = Vec::new();
    let mut consumed = 0;
    for message in messages {
        if message.role != ROLE_SYSTEM_MESSAGE {
            break;
        }
        instructions.push(text_parts(&parts(message.content.as_ref())?));
        consumed += 1;
    }
    Ok((instructions.join("\n\n"), consumed))
}

/// Projects the conversation after `prefix_len` system messages. `index` keeps
/// message ids stable against the client's original array.
fn project_history(
    messages: &[pb::InferenceCoreMessage],
    prefix_len: usize,
) -> Result<Vec<ProjectedMessage>> {
    let mut history = Vec::new();
    for (offset, message) in messages.iter().enumerate() {
        let index = prefix_len + offset;
        let role = message_role(message.role, index)?;
        if let Some(pb::inference_core_message::Content::ToolContent(content)) =
            message.content.as_ref()
        {
            history.extend(tool_results(index, &content.parts)?);
            continue;
        }
        let parts = parts(message.content.as_ref())?;
        let content = if role == Role::Assistant {
            assistant_content(message, &parts)?
        } else {
            ProjectedContent::Parts(parts)
        };
        history.push(ProjectedMessage {
            message_id: format!("cursor:{index}"),
            role,
            content,
        });
    }
    Ok(history)
}

fn message_role(role: i32, index: usize) -> Result<Role> {
    match role {
        ROLE_USER => Ok(Role::User),
        ROLE_ASSISTANT => Ok(Role::Assistant),
        ROLE_TOOL => Ok(Role::Tool),
        ROLE_SYSTEM_MESSAGE => Ok(Role::System),
        value => Err(Error::Protocol(format!(
            "RunInference message {index} has unsupported role {value}"
        ))),
    }
}

fn tool_results(
    index: usize,
    parts: &[pb::InferenceToolResultPart],
) -> Result<Vec<ProjectedMessage>> {
    let mut results = Vec::with_capacity(parts.len());
    for (part_index, part) in parts.iter().enumerate() {
        let provider_parts = parts_from_wire(&part.experimental_content)?;
        let content = part.result.as_ref().map_or(Value::Null, prost_json);
        results.push(ProjectedMessage {
            message_id: format!("cursor:{index}:tool:{part_index}"),
            role: Role::Tool,
            content: ProjectedContent::ToolResult(ToolResultContent {
                call_id: part.tool_call_id.clone(),
                name: part.tool_name.clone(),
                content: match &content {
                    Value::String(text) => text.clone(),
                    value => value.to_string(),
                },
                is_error: part.is_error,
                image: None,
                provider_parts,
            }),
        });
    }
    Ok(results)
}

fn assistant_content(
    message: &pb::InferenceCoreMessage,
    parts: &[ContentPart],
) -> Result<ProjectedContent> {
    let calls = message
        .tool_calls
        .iter()
        .enumerate()
        .map(|(index, call)| {
            let arguments = match &call.raw_tool_call_args {
                Some(raw) => serde_json::from_str(raw)?,
                None => call
                    .args
                    .as_ref()
                    .map_or_else(|| serde_json::json!({}), prost_struct),
            };
            Ok(ToolCallContent {
                index,
                call_id: call.tool_call_id.clone(),
                name: call.tool_name.clone(),
                arguments,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let thinking = message
        .reasoning_parts
        .iter()
        .map(|part| part.text.as_str())
        .collect::<String>();
    let blocks = message
        .reasoning_parts
        .iter()
        .filter_map(|part| {
            if let Some(signature) = &part.signature {
                Some(serde_json::json!({
                    "type": "thinking", "thinking": part.text, "signature": signature
                }))
            } else {
                part.redacted_data
                    .as_ref()
                    .map(|data| serde_json::json!({"type": "redacted_thinking", "data": data}))
            }
        })
        .collect::<Vec<_>>();
    let replay_state = (!blocks.is_empty()).then(|| ProviderReplayState {
        provider_kind: "anthropic".into(),
        value: serde_json::json!({"blocks": blocks}),
    });
    Ok(ProjectedContent::Assistant {
        text: text_parts(parts),
        thinking,
        replay_state,
        calls,
    })
}

fn subagent_models_task_description(
    name: &str,
    description: &str,
    models: &[PluginModelDescriptor],
) -> String {
    if models.is_empty() || !is_task_tool(name) {
        return description.to_owned();
    }
    let mut models = models.iter().collect::<Vec<_>>();
    models.sort_by(|left, right| {
        left.display_name
            .cmp(&right.display_name)
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut description = description.to_owned();
    description.push_str(
        "\n\nAdditional available subagent models (BYOK)\n\n\
         These configured models are also available for explicit model requests. \
         Pass the exact hash or the display name shown below as model; \
         the server resolves both to the same model.\n",
    );
    for model in models {
        let display_name = serde_json::json!(model.display_name);
        description.push_str("\n- ");
        description.push_str(&display_name.to_string());
        description.push_str(": model=\"");
        description.push_str(&model.id);
        description.push('"');
    }
    description
}

fn is_task_tool(name: &str) -> bool {
    name.eq_ignore_ascii_case("Task")
}

pub(crate) fn requested_context_window(
    requested: Option<&pb::InferenceRequestedModel>,
) -> Result<Option<u64>> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    requested
        .parameters
        .iter()
        .find(|parameter| parameter.id == "context")
        .map(|parameter| parse_context_tokens(&parameter.value))
        .transpose()
}

fn parse_context_tokens(value: &str) -> Result<u64> {
    parse_token_count(value)
        .ok_or_else(|| Error::Protocol(format!("invalid Cursor context token count: {value}")))
}

fn parts(content: Option<&pb::inference_core_message::Content>) -> Result<Vec<ContentPart>> {
    match content {
        Some(pb::inference_core_message::Content::Text(text)) => {
            Ok(vec![ContentPart::Text { text: text.clone() }])
        }
        Some(pb::inference_core_message::Content::Parts(parts)) => parts_from_wire(&parts.parts),
        Some(pb::inference_core_message::Content::ToolContent(_)) | None => Ok(Vec::new()),
    }
}

fn parts_from_wire(parts: &[pb::InferenceContentPart]) -> Result<Vec<ContentPart>> {
    parts
        .iter()
        .map(|part| match part.part.as_ref() {
            Some(pb::inference_content_part::Part::Text(text)) => Ok(ContentPart::Text {
                text: text.text.clone(),
            }),
            Some(pb::inference_content_part::Part::Image(image)) => {
                let (mime_type, data) = image_data(&image.data, image.mime_type.as_deref())?;
                Ok(ContentPart::Image { mime_type, data })
            }
            Some(pb::inference_content_part::Part::File(file)) => {
                let (mime_type, data) = file_data(&file.data, &file.media_type)?;
                Ok(ContentPart::File {
                    mime_type,
                    filename: file.filename.clone(),
                    data,
                })
            }
            None => Err(Error::Protocol("RunInference content part is empty".into())),
        })
        .collect()
}

fn image_data(value: &str, fallback_mime: Option<&str>) -> Result<(String, Vec<u8>)> {
    let (mime_type, encoded) = if let Some((metadata, data)) = value
        .strip_prefix("data:")
        .and_then(|value| value.split_once(','))
    {
        (metadata.trim_end_matches(";base64"), data)
    } else {
        (fallback_mime.unwrap_or("image/png"), value)
    };
    let data = STANDARD
        .decode(encoded)
        .map_err(|error| Error::Protocol(format!("invalid RunInference image: {error}")))?;
    Ok((mime_type.into(), data))
}

fn file_data(value: &str, fallback_mime: &str) -> Result<(String, Vec<u8>)> {
    let (mime_type, encoded) = if let Some((metadata, data)) = value
        .strip_prefix("data:")
        .and_then(|value| value.split_once(','))
    {
        (metadata.trim_end_matches(";base64"), data)
    } else {
        (fallback_mime, value)
    };
    if mime_type.is_empty() {
        return Err(Error::Protocol(
            "RunInference file content requires a media type".into(),
        ));
    }
    let data = STANDARD
        .decode(encoded)
        .map_err(|error| Error::Protocol(format!("invalid RunInference file: {error}")))?;
    Ok((mime_type.into(), data))
}

fn text_parts(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            ContentPart::Image { .. } | ContentPart::File { .. } => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use prost_types::{Struct, Value as ProtoValue, value::Kind};

    use super::*;

    #[test]
    fn sampling_config_projects_stop_sequences_out_of_extra_params() {
        let input = pb::InferenceStreamRequest {
            messages: vec![text(1, "hello")],
            model_config: Some(pb::InferenceModelConfig {
                temperature: Some(0.3),
                top_p: Some(0.7),
                stop_sequences: vec!["STOP".into(), "END".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let request = project(&input, "test-model", None, &[]).unwrap();
        assert_eq!(request.model.stop_sequences, vec!["STOP", "END"]);
        assert_eq!(request.model.extra_params["temperature"], 0.3_f32);
        assert_eq!(request.model.extra_params["top_p"], 0.7_f32);
        assert!(request.model.extra_params.get("stop").is_none());
    }

    #[test]
    fn unchanged_client_history_remains_an_exact_provider_prefix() {
        let mut input = pb::InferenceStreamRequest {
            messages: vec![text(4, "You are an assistant"), text(1, "First question")],
            ..Default::default()
        };
        let first = project(&input, "test-model", None, &[]).unwrap();
        input.messages.push(text(2, "First answer"));
        input.messages.push(text(1, "Second question"));
        let second = project(&input, "test-model", None, &[]).unwrap();

        assert_eq!(first.prompt.instructions, second.prompt.instructions);
        assert_eq!(first.history, second.history[..first.history.len()]);
    }

    #[test]
    fn tool_parameters_use_the_nested_json_schema() {
        let schema = Struct {
            fields: [(
                "type".into(),
                ProtoValue {
                    kind: Some(Kind::StringValue("object".into())),
                },
            )]
            .into_iter()
            .collect(),
        };
        let input = pb::InferenceStreamRequest {
            tools: vec![pb::InferenceAgentTool {
                name: "Shell".into(),
                parameters: Some(Struct {
                    fields: [(
                        "jsonSchema".into(),
                        ProtoValue {
                            kind: Some(Kind::StructValue(schema)),
                        },
                    )]
                    .into_iter()
                    .collect(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };

        let request = project(&input, "test-model", None, &[]).unwrap();
        assert_eq!(
            request.prompt.tools[0].parameters,
            serde_json::json!({"type":"object"})
        );
    }

    #[test]
    fn projects_file_parts_with_their_filename_media_type_and_bytes() {
        let input = pb::InferenceStreamRequest {
            messages: vec![pb::InferenceCoreMessage {
                role: 1,
                content: Some(pb::inference_core_message::Content::Parts(
                    pb::InferenceContentParts {
                        parts: vec![pb::InferenceContentPart {
                            part: Some(pb::inference_content_part::Part::File(
                                pb::InferenceFilePart {
                                    data: "data:application/pdf;base64,JVBERi0x".into(),
                                    media_type: "application/octet-stream".into(),
                                    filename: Some("report.pdf".into()),
                                    ..Default::default()
                                },
                            )),
                        }],
                    },
                )),
                ..Default::default()
            }],
            ..Default::default()
        };
        let request = project(&input, "test-model", None, &[]).unwrap();
        assert_eq!(
            request.history[0].content,
            ProjectedContent::Parts(vec![ContentPart::File {
                mime_type: "application/pdf".into(),
                filename: Some("report.pdf".into()),
                data: b"%PDF-1".to_vec(),
            }])
        );
    }

    fn text(role: i32, content: &str) -> pb::InferenceCoreMessage {
        pb::InferenceCoreMessage {
            role,
            content: Some(pb::inference_core_message::Content::Text(content.into())),
            ..Default::default()
        }
    }
}
