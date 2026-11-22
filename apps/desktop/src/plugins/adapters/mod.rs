//! Plugin protocol adapters: request bodies, HTTP/SSE, and normalized events.

mod anthropic;
mod chat;
mod responses;

use std::collections::HashMap;

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{
    Error, Result,
    worker::{HostContext, InvocationState},
};
use crate::provider::ModelEvent;

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_STREAM_BYTES: u64 = 256 * 1024 * 1024;

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
    let request = invocation.llm_request()?;
    let body = match protocol {
        ProtocolKind::OpenAiChat => chat::build_body(&input.model, &request)?,
        ProtocolKind::OpenAiResponses => responses::build_body(&input.model, &request)?,
        ProtocolKind::Anthropic => anthropic::build_body(&input.model, &request)?,
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
        let body = read_error_body(&mut response, &cancellation, recorder.as_ref()).await?;
        return Err(Error::HttpStatus { status, body });
    }
    let mut parser: Box<dyn ProtocolParser> = match protocol {
        ProtocolKind::OpenAiChat => Box::new(chat::Parser::new()),
        ProtocolKind::OpenAiResponses => Box::new(responses::Parser::new()),
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
    fn push_data(&mut self, payload: &str, emit: &mut dyn FnMut(ModelEvent)) -> Result<bool>;
    fn finish(&mut self, emit: &mut dyn FnMut(ModelEvent)) -> Result<()>;
}

async fn send_cancellable(
    request: reqwest::RequestBuilder,
    cancellation: &CancellationToken,
) -> Result<reqwest::Response> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(Error::Cancelled),
        response = request.send() => {
            response.map_err(|_| Error::Provider("plugin network request failed".into()))
        }
    }
}

async fn read_error_body(
    response: &mut reqwest::Response,
    cancellation: &CancellationToken,
    recorder: Option<&crate::calls::CallRecorder>,
) -> Result<String> {
    let mut body = Vec::new();
    loop {
        let chunk = tokio::select! {
            _ = cancellation.cancelled() => return Err(Error::Cancelled),
            chunk = response.chunk() => {
                chunk.map_err(|_| Error::Provider("cannot read plugin network response".into()))?
            }
        };
        let Some(chunk) = chunk else { break };
        if let Some(recorder) = recorder {
            recorder.response_chunk(&chunk).await?;
        }
        let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(body.len());
        if remaining > 0 {
            body.extend_from_slice(&chunk[..remaining.min(chunk.len())]);
        }
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
    let mut total = 0_u64;
    loop {
        let chunk = tokio::select! {
            _ = cancellation.cancelled() => return Err(Error::Cancelled),
            chunk = body.next() => chunk,
        };
        let Some(chunk) = chunk else { break };
        let chunk = chunk.map_err(|_| Error::Provider("cannot read plugin network stream".into()))?;
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
        while let Some(position) = buffered.iter().position(|byte| *byte == b'\n') {
            let mut line = buffered.drain(..=position).collect::<Vec<u8>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if push_line(
                parser,
                &String::from_utf8_lossy(&line),
                &mut emit,
            )? {
                return Ok(());
            }
        }
    }
    if !buffered.is_empty() {
        push_line(
            parser,
            &String::from_utf8_lossy(&buffered),
            &mut emit,
        )?;
    }
    Ok(())
}

fn push_line(
    parser: &mut dyn ProtocolParser,
    line: &str,
    emit: &mut dyn FnMut(ModelEvent),
) -> Result<bool> {
    let Some(payload) = sse_payload(line) else {
        return Ok(false);
    };
    parser.push_data(payload, emit)
}

fn sse_payload(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("data:")?;
    let payload = rest.trim();
    if payload.is_empty() {
        None
    } else {
        Some(payload)
    }
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

#[cfg(test)]
fn collect_events(
    parser: &mut dyn ProtocolParser,
    payloads: &[&str],
) -> Result<Vec<ModelEvent>> {
    let mut events = Vec::new();
    for payload in payloads {
        if parser.push_data(payload, &mut |event| events.push(event))? {
            break;
        }
    }
    parser.finish(&mut |event| events.push(event))?;
    Ok(events)
}

#[cfg(test)]
mod tests {
    use crate::plugins::wire::{LlmContentPart, LlmMessage, LlmReasoning, LlmRequest};

    pub(crate) fn request() -> LlmRequest {
        LlmRequest {
            instructions: "You are concise.".into(),
            messages: vec![LlmMessage::User {
                content: vec![LlmContentPart::Text {
                    text: "hello".into(),
                }],
            }],
            tools: Vec::new(),
            reasoning: LlmReasoning {
                enabled: false,
                effort: None,
            },
            latency: "standard".into(),
            max_output_tokens: None,
            cache_key: Some("conversation-1".into()),
        }
    }
}
