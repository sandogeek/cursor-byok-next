//! Connect frames, compressed payloads, and Cursor protobuf views.

use std::io::{self, Read};
use std::sync::{Arc, LazyLock, Mutex};

use brotli::Decompressor;
use flate2::read::{GzDecoder, ZlibDecoder};
use prost_reflect::{DescriptorPool, DynamicMessage, MethodDescriptor, ReflectMessage, Value};
use serde::Serialize;

use crate::config::{DEFAULT_MAX_FRAMES, MAX_CONNECT_FRAME_BYTES};
use crate::types::{Exchange, FrameView, Payload};

pub const BIDI_APPEND_PATH: &str = "/aiserver.v1.BidiService/BidiAppend";
pub const FORK_BACKGROUND_COMPOSER_PATH: &str =
    "/aiserver.v1.BackgroundComposerService/ForkBackgroundComposer";
pub const NOTIFY_CONVERSATION_CLONE_PATH: &str = "/agent.v1.AgentService/NotifyConversationClone";
pub const UPLOAD_CONVERSATION_BLOBS_PATH: &str = "/agent.v1.AgentService/UploadConversationBlobs";
pub const CPP_AVAILABLE_MODELS_PATH: &str = "/aiserver.v1.CppService/AvailableModels";
pub const AI_AVAILABLE_MODELS_PATH: &str = "/aiserver.v1.AiService/AvailableModels";
pub const AI_GET_DEFAULT_MODEL_PATH: &str = "/aiserver.v1.AiService/GetDefaultModel";
pub const AI_DEFAULT_MODEL_NUDGE_DATA_PATH: &str =
    "/aiserver.v1.AiService/GetDefaultModelNudgeData";
pub const MCP_GET_KNOWN_SERVERS_PATH: &str = "/aiserver.v1.MCPRegistryService/GetKnownServers";
pub const SERVER_GET_CONFIG_PATH: &str = "/aiserver.v1.ServerConfigService/GetServerConfig";
pub const RUN_SSE_PATH: &str = "/agent.v1.AgentService/RunSSE";

static POOL: LazyLock<DescriptorPool> = LazyLock::new(|| {
    DescriptorPool::decode(CURSOR_DESCRIPTORS).expect("decode Cursor protobuf descriptors")
});

/// Frame and message lengths travel over the wire as `i32`. Cursor never sends
/// a frame past `MAX_CONNECT_FRAME_BYTES`, so anything larger is clamped
/// instead of wrapping into a negative length.
fn payload_length(len: usize) -> i32 {
    i32::try_from(len).unwrap_or(i32::MAX)
}

pub fn frame_count(len: usize) -> i32 {
    i32::try_from(len).unwrap_or(i32::MAX)
}

/// A capture limit stored as `i32`, as the `usize` a buffer needs.
pub fn capture_limit(limit: i32) -> usize {
    usize::try_from(limit.max(0)).unwrap_or(usize::MAX)
}

/// One byte past the decompressed-size cap, so `io::copy` can detect an
/// oversized payload rather than truncating it.
fn frame_limit_bytes() -> u64 {
    u64::try_from(MAX_CONNECT_FRAME_BYTES.saturating_add(1)).unwrap_or(u64::MAX)
}

static CURSOR_DESCRIPTORS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cursor.bin"));

static PROTO_JSON: LazyLock<prost_reflect::SerializeOptions> = LazyLock::new(|| {
    prost_reflect::SerializeOptions::new()
        .use_proto_field_name(true)
        .skip_default_fields(true)
});

pub struct ConnectFrameDecoder {
    buffer: Vec<u8>,
    message_type: String,
    codec: String,
    max_frames: i32,
    frame_count: i32,
    on_frame: Box<dyn FnMut(FrameView) + Send>,
}

impl ConnectFrameDecoder {
    pub fn new(
        message_type: impl Into<String>,
        codec: impl Into<String>,
        max_frames: i32,
        on_frame: impl FnMut(FrameView) + Send + 'static,
    ) -> Self {
        Self {
            buffer: Vec::new(),
            message_type: message_type.into(),
            codec: codec.into().trim().to_string(),
            max_frames,
            frame_count: 0,
            on_frame: Box::new(on_frame),
        }
    }

    pub fn write(&mut self, payload: &[u8]) {
        if payload.is_empty() || self.frame_count >= self.max_frames {
            return;
        }
        self.buffer.extend_from_slice(payload);
        while self.buffer.len() >= 5 && self.frame_count < self.max_frames {
            let flags = self.buffer[0];
            let length = usize::try_from(u32::from_be_bytes(
                self.buffer[1..5].try_into().expect("4 bytes"),
            ))
            .unwrap_or(usize::MAX);
            if length > MAX_CONNECT_FRAME_BYTES {
                self.emit(FrameView {
                    flags,
                    length: payload_length(length),
                    error: "Connect 帧长度异常".into(),
                    ..empty_frame()
                });
                self.buffer.clear();
                return;
            }
            if self.buffer.len() < 5 + length {
                return;
            }
            let frame_payload = self.buffer[5..5 + length].to_vec();
            self.buffer.drain(..5 + length);
            let frame = self.decode(flags, &frame_payload);
            self.emit(frame);
        }
    }

    pub fn close(&mut self) {
        if !self.buffer.is_empty() && self.frame_count < self.max_frames {
            self.emit(FrameView {
                length: payload_length(self.buffer.len()),
                raw_hex: clipped_hex(&self.buffer, 4096),
                error: "流结束时仍有不完整的 Connect 帧".into(),
                ..empty_frame()
            });
        }
        self.buffer.clear();
    }

    fn emit(&mut self, mut frame: FrameView) {
        frame.index = self.frame_count;
        self.frame_count += 1;
        (self.on_frame)(frame);
    }

    fn decode(&self, flags: u8, payload: &[u8]) -> FrameView {
        let mut frame = FrameView {
            flags,
            length: payload_length(payload.len()),
            compressed: flags & 0x01 != 0,
            end_stream: flags & 0x02 != 0,
            raw_hex: clipped_hex(payload, 4096),
            ..empty_frame()
        };
        let decoded = if frame.compressed {
            match decompress_payload(payload, &self.codec) {
                Ok(bytes) => bytes,
                Err(err) => {
                    frame.error = err;
                    return frame;
                }
            }
        } else {
            payload.to_vec()
        };
        if frame.end_stream {
            frame.kind = "end_stream".into();
            frame.message_type = "connect.error.v1.EndStreamResponse".into();
            frame.json = pretty_json(&decoded);
            return frame;
        }
        match decode_named_message(&self.message_type, &decoded) {
            None => frame.error = "未知的 protobuf 消息类型".into(),
            Some(Err(err)) => frame.error = format!("protobuf 解码失败：{err}"),
            Some(Ok(message)) => {
                frame.message_type.clone_from(&self.message_type);
                frame.kind = active_oneof_name(&message);
                if self.message_type == "aiserver.v1.BidiRequestId" {
                    frame.request_id = string_field(&message, "request_id");
                }
                frame.json = marshal_proto_json(&message);
            }
        }
        frame
    }
}

#[derive(Debug, Default)]
pub struct DecodedUnary {
    pub json: String,
    pub kind: String,
    pub request_id: String,
    pub conversation_id: String,
    pub error: Option<String>,
}

pub fn decode_unary_request(path: &str, payload: &[u8]) -> DecodedUnary {
    if path == BIDI_APPEND_PATH {
        return decode_bidi_append_request(payload);
    }
    let Some((descriptor, kind)) = unary_request_message(path) else {
        return DecodedUnary::default();
    };
    match DynamicMessage::decode(descriptor, payload) {
        Ok(message) => DecodedUnary {
            json: marshal_proto_json(&message),
            kind,
            conversation_id: conversation_id_from_unary_request(&message),
            ..DecodedUnary::default()
        },
        Err(err) => DecodedUnary {
            error: Some(err.to_string()),
            ..DecodedUnary::default()
        },
    }
}

fn decode_bidi_append_request(payload: &[u8]) -> DecodedUnary {
    let request = match decode_named_message("aiserver.v1.BidiAppendRequest", payload) {
        Some(Ok(request)) => request,
        Some(Err(err)) => {
            return DecodedUnary {
                error: Some(err.to_string()),
                ..DecodedUnary::default()
            };
        }
        None => {
            return DecodedUnary {
                error: Some("未知的 protobuf 消息类型".into()),
                ..DecodedUnary::default()
            };
        }
    };
    let request_id = message_field(&request, "request_id")
        .map(|id| string_field(&id, "request_id"))
        .unwrap_or_default();
    let outer = marshal_proto_json(&request);
    match decode_bidi_client_message(&request) {
        Err(err) => DecodedUnary {
            json: outer,
            kind: "bidi_append".into(),
            request_id,
            error: Some(err),
            ..DecodedUnary::default()
        },
        Ok(None) => DecodedUnary {
            json: outer,
            kind: "bidi_append".into(),
            request_id,
            ..DecodedUnary::default()
        },
        Ok(Some((client, kind))) => {
            let conversation_id = conversation_id_from_client_message(&client);
            let combined = BidiAppendView {
                bidi_append_request: serde_json::from_str(&outer)
                    .unwrap_or(serde_json::Value::Null),
                agent_client_kind: kind.clone(),
                agent_client_message: serde_json::from_str(&marshal_proto_json(&client))
                    .unwrap_or(serde_json::Value::Null),
            };
            match serde_json::to_string_pretty(&combined) {
                Ok(formatted) => DecodedUnary {
                    json: formatted,
                    kind,
                    request_id,
                    conversation_id,
                    error: None,
                },
                Err(err) => DecodedUnary {
                    json: outer,
                    kind,
                    request_id,
                    conversation_id,
                    error: Some(err.to_string()),
                },
            }
        }
    }
}

#[derive(Serialize)]
struct BidiAppendView {
    bidi_append_request: serde_json::Value,
    agent_client_kind: String,
    agent_client_message: serde_json::Value,
}

fn decode_bidi_client_message(
    request: &DynamicMessage,
) -> Result<Option<(DynamicMessage, String)>, String> {
    let data = string_field(request, "data");
    if !data.trim().is_empty() {
        let payload = hex::decode(data.trim())
            .map_err(|err| format!("decode hex agent client message failed: {err}"))?;
        let message = decode_named_message("agent.v1.AgentClientMessage", &payload)
            .ok_or_else(|| "未知的 protobuf 消息类型".to_string())?
            .map_err(|err| format!("decode agent client message failed: {err}"))?;
        let kind = active_oneof_name(&message);
        return Ok(Some((message, kind)));
    }
    let binary = bytes_field(request, "data_binary");
    if binary.is_empty() {
        return Ok(None);
    }
    let message = decode_named_message("agent.v1.AgentClientMessage", &binary)
        .ok_or_else(|| "未知的 protobuf 消息类型".to_string())?
        .map_err(|err| format!("decode binary agent client message failed: {err}"))?;
    let kind = active_oneof_name(&message);
    Ok(Some((message, kind)))
}

fn conversation_id_from_client_message(message: &DynamicMessage) -> String {
    if let Some(run_request) = message_field(message, "run_request") {
        return string_field(&run_request, "conversation_id");
    }
    if let Some(prewarm_request) = message_field(message, "prewarm_request") {
        return string_field(&prewarm_request, "conversation_id");
    }
    String::new()
}

fn conversation_id_from_unary_request(message: &DynamicMessage) -> String {
    match message.descriptor().full_name() {
        "agent.v1.NotifyConversationCloneRequest" | "agent.v1.UploadConversationBlobsRequest" => {
            string_field(message, "conversation_id")
        }
        _ => String::new(),
    }
}

pub fn decode_unary_response(path: &str, payload: &[u8]) -> Result<(String, String), String> {
    let Some((descriptor, kind)) = unary_response_message(path) else {
        return Ok(Default::default());
    };
    let message = DynamicMessage::decode(descriptor, payload).map_err(|err| err.to_string())?;
    Ok((marshal_proto_json(&message), kind))
}

pub fn hydrate_stored_exchange(exchange: &mut Exchange) -> bool {
    if exchange.summary.state != "completed" && exchange.summary.state != "streaming" {
        return false;
    }
    let request_frames = hydrate_stored_request_frames(exchange);
    let response_frames = hydrate_stored_response_frames(exchange);
    let request_unary = hydrate_stored_unary_request(exchange);
    let response_unary = hydrate_stored_unary_response(exchange);
    let request_text = hydrate_stored_request_text(exchange);
    let response_text = hydrate_stored_response_text(exchange);
    request_frames
        || response_frames
        || request_unary
        || response_unary
        || request_text
        || response_text
}

fn hydrate_stored_request_frames(exchange: &mut Exchange) -> bool {
    let Some(message_type) = streaming_request_message_type(&exchange.summary.path) else {
        return false;
    };
    if !exchange.request.frames.is_empty()
        || exchange.request.raw_hex.is_empty()
        || exchange.request.raw_truncated
    {
        return false;
    }
    match decode_stored_connect_frames(
        &exchange.request.raw_hex,
        &message_type,
        &exchange.request.content_codec,
    ) {
        Ok(frames) if !frames.is_empty() => {
            for frame in &frames {
                if !frame.kind.is_empty() && frame.kind != "end_stream" {
                    exchange.summary.request_kind.clone_from(&frame.kind);
                }
                if !frame.request_id.is_empty() {
                    exchange.summary.request_id.clone_from(&frame.request_id);
                }
            }
            exchange.request.frames = frames;
        }
        Ok(_) => {}
        Err(err) => exchange.request.decode_error = err,
    }
    true
}

fn hydrate_stored_response_frames(exchange: &mut Exchange) -> bool {
    let Some(message_type) = streaming_response_message_type(&exchange.summary.path) else {
        return false;
    };
    if !exchange.response.frames.is_empty()
        || exchange.response.raw_hex.is_empty()
        || exchange.response.raw_truncated
    {
        return false;
    }
    match decode_stored_connect_frames(
        &exchange.response.raw_hex,
        &message_type,
        &exchange.response.content_codec,
    ) {
        Ok(frames) if !frames.is_empty() => {
            exchange.summary.frame_count = frame_count(frames.len());
            for frame in &frames {
                if !frame.kind.is_empty() && frame.kind != "end_stream" {
                    exchange.summary.response_kind.clone_from(&frame.kind);
                }
            }
            exchange.response.frames = frames;
        }
        Ok(_) => {}
        Err(err) => exchange.response.decode_error = err,
    }
    true
}

fn hydrate_stored_unary_request(exchange: &mut Exchange) -> bool {
    if !is_unary_proto_content_type(&exchange.request.content_type)
        || !exchange.request.decoded_json.is_empty()
        || exchange.request.raw_truncated
    {
        return false;
    }
    match decode_stored_raw_payload(&exchange.request.raw_hex, &exchange.request.content_codec) {
        Ok(payload) => {
            let decoded = decode_unary_request(&exchange.summary.path, &payload);
            if let Some(err) = decoded.error {
                exchange.request.decode_error = err;
            }
            if decoded.json.is_empty() {
                return !exchange.request.decode_error.is_empty();
            }
            exchange.request.decoded_json = decoded.json;
            exchange.request.decoded_lang = "json".into();
            exchange.summary.request_kind = decoded.kind;
            if !decoded.request_id.is_empty() {
                exchange.summary.request_id = decoded.request_id;
            }
            if !decoded.conversation_id.is_empty() {
                exchange.summary.conversation_id = decoded.conversation_id;
            }
        }
        Err(err) => exchange.request.decode_error = err,
    }
    true
}

fn hydrate_stored_unary_response(exchange: &mut Exchange) -> bool {
    if !is_unary_proto_content_type(&exchange.response.content_type)
        || !exchange.response.decoded_json.is_empty()
        || exchange.response.raw_truncated
    {
        return false;
    }
    match decode_stored_raw_payload(&exchange.response.raw_hex, &exchange.response.content_codec) {
        Ok(payload) => match decode_unary_response(&exchange.summary.path, &payload) {
            Ok((decoded, kind)) if !decoded.is_empty() => {
                exchange.response.decoded_json = decoded;
                exchange.response.decoded_lang = "json".into();
                exchange.summary.response_kind = kind;
            }
            Ok(_) => {}
            Err(err) => exchange.response.decode_error = err,
        },
        Err(err) => exchange.response.decode_error = err,
    }
    true
}

fn hydrate_stored_request_text(exchange: &mut Exchange) -> bool {
    exchange.request.decoded_json.is_empty()
        && !exchange.request.raw_hex.is_empty()
        && !is_proto_content_type(&exchange.request.content_type)
        && streaming_request_message_type(&exchange.summary.path).is_none()
        && hydrate_stored_text_payload(&mut exchange.request)
}

fn hydrate_stored_response_text(exchange: &mut Exchange) -> bool {
    exchange.response.decoded_json.is_empty()
        && !exchange.response.raw_hex.is_empty()
        && !is_proto_content_type(&exchange.response.content_type)
        && streaming_response_message_type(&exchange.summary.path).is_none()
        && hydrate_stored_text_payload(&mut exchange.response)
}

fn hydrate_stored_text_payload(payload: &mut Payload) -> bool {
    let raw = match hex::decode(payload.raw_hex.trim()) {
        Ok(raw) => raw,
        Err(err) => {
            payload.decode_error = format!("解析已存储正文失败：{err}");
            return true;
        }
    };
    let decoded = decode_captured_content(&raw, &payload.content_type, &payload.content_codec);
    if decoded.text.is_empty() && decoded.error.is_none() {
        return false;
    }
    payload.decoded_json = decoded.text;
    payload.decoded_lang = decoded.language;
    if let Some(err) = decoded.error {
        payload.decode_error = err;
    }
    true
}

#[derive(Debug, Default)]
pub struct DecodedContent {
    pub text: String,
    pub language: String,
    pub error: Option<String>,
}

pub fn decode_captured_content(payload: &[u8], content_type: &str, codec: &str) -> DecodedContent {
    let decoded = match decode_http_content(payload, codec) {
        Ok(decoded) => decoded,
        Err(err) => {
            return DecodedContent {
                error: Some(err),
                ..DecodedContent::default()
            };
        }
    };
    if decoded.is_empty() {
        return DecodedContent::default();
    }
    let media_type = normalized_media_type(content_type);
    if json_valid(&decoded) {
        return match serde_json::from_slice::<serde_json::Value>(&decoded)
            .and_then(|value| serde_json::to_string_pretty(&value))
        {
            Ok(formatted) => DecodedContent {
                text: formatted,
                language: "json".into(),
                error: None,
            },
            Err(err) => DecodedContent {
                text: String::from_utf8_lossy(&decoded).into_owned(),
                language: "json".into(),
                error: Some(err.to_string()),
            },
        };
    }
    if media_type == "application/x-www-form-urlencoded" && std::str::from_utf8(&decoded).is_ok() {
        let mut values: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for (key, value) in url::form_urlencoded::parse(&decoded) {
            values
                .entry(key.into_owned())
                .or_default()
                .push(value.into_owned());
        }
        return match serde_json::to_string_pretty(&values) {
            Ok(formatted) => DecodedContent {
                text: formatted,
                language: "json".into(),
                error: None,
            },
            Err(err) => DecodedContent {
                text: String::from_utf8_lossy(&decoded).into_owned(),
                language: "plaintext".into(),
                error: Some(err.to_string()),
            },
        };
    }
    if !is_text_media_type(&media_type) || std::str::from_utf8(&decoded).is_err() {
        return DecodedContent::default();
    }
    let text = String::from_utf8_lossy(&decoded);
    if text.contains('\0') {
        return DecodedContent::default();
    }
    let language = text_language(&media_type);
    if media_type.ends_with("+json") || media_type == "application/json" {
        return DecodedContent {
            text: text.into_owned(),
            language: "json".into(),
            error: Some("JSON 正文格式无效".into()),
        };
    }
    DecodedContent {
        text: text.into_owned(),
        language,
        error: None,
    }
}

fn json_valid(payload: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(payload).is_ok()
}

/// Reads a decompressor to the end, refusing anything past the frame cap.
fn inflate_limited(mut reader: impl Read, label: &str) -> Result<Vec<u8>, String> {
    let mut result = Vec::new();
    io::copy(&mut reader.by_ref().take(frame_limit_bytes()), &mut result)
        .map_err(|err| format!("读取 {label} 内容失败：{err}"))?;
    if result.len() > MAX_CONNECT_FRAME_BYTES {
        return Err(format!(
            "{label} 解压后超过 {MAX_CONNECT_FRAME_BYTES} 字节限制"
        ));
    }
    Ok(result)
}

pub fn decode_http_content(payload: &[u8], codec: &str) -> Result<Vec<u8>, String> {
    let encodings: Vec<_> = codec
        .split(',')
        .map(|item| item.trim().to_ascii_lowercase())
        .collect();
    let mut decoded = payload.to_vec();
    for encoding in encodings.into_iter().rev() {
        match encoding.as_str() {
            "" | "identity" => {}
            "gzip" | "x-gzip" => decoded = decompress_payload(&decoded, "gzip")?,
            "deflate" => {
                let inflater = ZlibDecoder::new(decoded.as_slice());
                decoded = inflate_limited(inflater, "deflate")?;
            }
            "br" => {
                let decompressor = Decompressor::new(decoded.as_slice(), 4096);
                decoded = inflate_limited(decompressor, "Brotli")?;
            }
            other => return Err(format!("暂不支持内容编码 {other:?}")),
        }
    }
    Ok(decoded)
}

pub fn normalized_media_type(content_type: &str) -> String {
    let trimmed = content_type.trim();
    trimmed.parse::<mime::Mime>().map_or_else(
        |_| {
            trimmed
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        },
        |mime| mime.essence_str().to_ascii_lowercase(),
    )
}

pub fn is_proto_content_type(content_type: &str) -> bool {
    normalized_media_type(content_type).contains("proto")
}

fn is_text_media_type(media_type: &str) -> bool {
    media_type.starts_with("text/")
        || media_type.ends_with("+json")
        || media_type.ends_with("+xml")
        || media_type == "application/json"
        || media_type == "application/xml"
        || media_type == "application/javascript"
        || media_type == "application/x-javascript"
        || media_type == "application/graphql"
}

fn text_language(media_type: &str) -> String {
    if media_type.contains("json") {
        "json"
    } else if media_type.contains("xml") {
        "xml"
    } else if media_type.contains("html") {
        "html"
    } else if media_type.contains("javascript") {
        "javascript"
    } else if media_type.contains("css") {
        "css"
    } else {
        "plaintext"
    }
    .into()
}

pub fn decode_stored_connect_frames(
    raw_hex_value: &str,
    message_type: &str,
    codec: &str,
) -> Result<Vec<FrameView>, String> {
    let payload = hex::decode(raw_hex_value.trim())
        .map_err(|err| format!("解析已存储 Connect 正文失败：{err}"))?;
    let frames = Arc::new(Mutex::new(Vec::new()));
    let collected = Arc::clone(&frames);
    let mut decoder =
        ConnectFrameDecoder::new(message_type, codec, DEFAULT_MAX_FRAMES, move |frame| {
            collected.lock().expect("frame lock").push(frame);
        });
    decoder.write(&payload);
    decoder.close();
    let frames = frames.lock().expect("frame lock");
    Ok(frames.clone())
}

pub fn is_unary_proto_content_type(content_type: &str) -> bool {
    matches!(
        normalized_media_type(content_type).as_str(),
        "application/proto" | "application/protobuf" | "application/x-protobuf"
    )
}

pub fn decode_stored_raw_payload(raw_hex_value: &str, codec: &str) -> Result<Vec<u8>, String> {
    let payload =
        hex::decode(raw_hex_value.trim()).map_err(|err| format!("解析已存储正文失败：{err}"))?;
    if codec.is_empty() || codec.eq_ignore_ascii_case("identity") {
        return Ok(payload);
    }
    decompress_payload(&payload, codec)
}

fn unary_request_message(path: &str) -> Option<(prost_reflect::MessageDescriptor, String)> {
    let mapped = match path {
        FORK_BACKGROUND_COMPOSER_PATH => Some((
            "aiserver.v1.ForkBackgroundComposerRequest",
            "fork_background_composer_request",
        )),
        NOTIFY_CONVERSATION_CLONE_PATH => Some((
            "agent.v1.NotifyConversationCloneRequest",
            "notify_conversation_clone_request",
        )),
        UPLOAD_CONVERSATION_BLOBS_PATH => Some((
            "agent.v1.UploadConversationBlobsRequest",
            "upload_conversation_blobs_request",
        )),
        CPP_AVAILABLE_MODELS_PATH => Some((
            "aiserver.v1.AvailableCppModelsRequest",
            "available_cpp_models_request",
        )),
        AI_AVAILABLE_MODELS_PATH => Some((
            "aiserver.v1.AvailableModelsRequest",
            "available_models_request",
        )),
        AI_GET_DEFAULT_MODEL_PATH => Some((
            "aiserver.v1.GetDefaultModelRequest",
            "get_default_model_request",
        )),
        AI_DEFAULT_MODEL_NUDGE_DATA_PATH => Some((
            "aiserver.v1.GetDefaultModelNudgeDataRequest",
            "get_default_model_nudge_data_request",
        )),
        MCP_GET_KNOWN_SERVERS_PATH => Some((
            "aiserver.v1.GetKnownServersRequest",
            "get_known_servers_request",
        )),
        SERVER_GET_CONFIG_PATH => Some((
            "aiserver.v1.GetServerConfigRequest",
            "get_server_config_request",
        )),
        _ => None,
    };
    if let Some((name, kind)) = mapped {
        return POOL
            .get_message_by_name(name)
            .map(|descriptor| (descriptor, kind.into()));
    }
    let method = rpc_method_descriptor(path)?;
    if method.is_client_streaming() || method.is_server_streaming() {
        return None;
    }
    Some((method.input(), proto_message_kind(method.input().name())))
}

fn unary_response_message(path: &str) -> Option<(prost_reflect::MessageDescriptor, String)> {
    let mapped = match path {
        FORK_BACKGROUND_COMPOSER_PATH => Some((
            "aiserver.v1.ForkBackgroundComposerResponse",
            "fork_background_composer_response",
        )),
        NOTIFY_CONVERSATION_CLONE_PATH => Some((
            "agent.v1.NotifyConversationCloneResponse",
            "notify_conversation_clone_response",
        )),
        UPLOAD_CONVERSATION_BLOBS_PATH => Some((
            "agent.v1.UploadConversationBlobsResponse",
            "upload_conversation_blobs_response",
        )),
        CPP_AVAILABLE_MODELS_PATH => Some((
            "aiserver.v1.AvailableCppModelsResponse",
            "available_cpp_models_response",
        )),
        AI_AVAILABLE_MODELS_PATH => Some((
            "aiserver.v1.AvailableModelsResponse",
            "available_models_response",
        )),
        AI_GET_DEFAULT_MODEL_PATH => Some((
            "aiserver.v1.GetDefaultModelResponse",
            "get_default_model_response",
        )),
        AI_DEFAULT_MODEL_NUDGE_DATA_PATH => Some((
            "aiserver.v1.GetDefaultModelNudgeDataResponse",
            "get_default_model_nudge_data_response",
        )),
        MCP_GET_KNOWN_SERVERS_PATH => Some((
            "aiserver.v1.GetKnownServersResponse",
            "get_known_servers_response",
        )),
        SERVER_GET_CONFIG_PATH => Some((
            "aiserver.v1.GetServerConfigResponse",
            "get_server_config_response",
        )),
        _ => None,
    };
    if let Some((name, kind)) = mapped {
        return POOL
            .get_message_by_name(name)
            .map(|descriptor| (descriptor, kind.into()));
    }
    let method = rpc_method_descriptor(path)?;
    if method.is_client_streaming() || method.is_server_streaming() {
        return None;
    }
    Some((method.output(), proto_message_kind(method.output().name())))
}

pub fn streaming_request_message_type(path: &str) -> Option<String> {
    if path == RUN_SSE_PATH {
        return Some("aiserver.v1.BidiRequestId".into());
    }
    let method = rpc_method_descriptor(path)?;
    if !method.is_client_streaming() && !method.is_server_streaming() {
        return None;
    }
    Some(method.input().full_name().to_string())
}

pub fn streaming_response_message_type(path: &str) -> Option<String> {
    if path == RUN_SSE_PATH {
        return Some("agent.v1.AgentServerMessage".into());
    }
    let method = rpc_method_descriptor(path)?;
    if !method.is_client_streaming() && !method.is_server_streaming() {
        return None;
    }
    Some(method.output().full_name().to_string())
}

pub fn decodes_unary_request(path: &str) -> bool {
    path == BIDI_APPEND_PATH || unary_request_message(path).is_some()
}

pub fn decodes_unary_response(path: &str) -> bool {
    unary_response_message(path).is_some()
}

fn decode_named_message(
    message_type: &str,
    payload: &[u8],
) -> Option<Result<DynamicMessage, prost::DecodeError>> {
    POOL.get_message_by_name(message_type)
        .map(|descriptor| DynamicMessage::decode(descriptor, payload))
}

fn rpc_method_descriptor(path: &str) -> Option<MethodDescriptor> {
    let parts: Vec<_> = path.trim().trim_matches('/').split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return None;
    }
    let service = POOL.get_service_by_name(parts[0])?;
    service.methods().find(|method| method.name() == parts[1])
}

fn proto_message_kind(name: &str) -> String {
    snake_case(name)
}

pub fn snake_case(value: &str) -> String {
    let mut result = String::new();
    for (index, character) in value.chars().enumerate() {
        if character.is_uppercase() {
            if index > 0 {
                result.push('_');
            }
            result.extend(character.to_lowercase());
            continue;
        }
        result.push(character);
    }
    result
}

pub fn marshal_proto_json(message: &DynamicMessage) -> String {
    let mut buf = Vec::new();
    let mut serializer = serde_json::Serializer::pretty(&mut buf);
    if message
        .serialize_with_options(&mut serializer, &PROTO_JSON)
        .is_err()
    {
        return String::new();
    }
    String::from_utf8(buf).unwrap_or_default()
}

pub fn active_oneof_name(message: &DynamicMessage) -> String {
    for oneof in message.descriptor().oneofs() {
        for field in oneof.fields() {
            if message.has_field(&field) {
                return field.name().to_string();
            }
        }
    }
    message.descriptor().name().to_string()
}

pub fn pretty_json(payload: &[u8]) -> String {
    match serde_json::from_slice::<serde_json::Value>(payload) {
        Ok(value) => serde_json::to_string_pretty(&value)
            .unwrap_or_else(|_| String::from_utf8_lossy(payload).into_owned()),
        Err(_) => String::from_utf8_lossy(payload).into_owned(),
    }
}

pub fn clipped_hex(payload: &[u8], max: usize) -> String {
    if payload.len() > max {
        format!("{}...", hex::encode(&payload[..max]))
    } else {
        hex::encode(payload)
    }
}

pub fn raw_hex(payload: &[u8]) -> String {
    hex::encode(payload)
}

pub fn decompress_payload(payload: &[u8], codec: &str) -> Result<Vec<u8>, String> {
    if !codec.is_empty() && !codec.eq_ignore_ascii_case("gzip") {
        return Err(format!("暂不支持压缩算法 {codec:?}"));
    }
    inflate_limited(GzDecoder::new(payload), "gzip")
}

fn empty_frame() -> FrameView {
    FrameView {
        index: 0,
        flags: 0,
        length: 0,
        compressed: false,
        end_stream: false,
        kind: String::new(),
        message_type: String::new(),
        request_id: String::new(),
        json: String::new(),
        raw_hex: String::new(),
        error: String::new(),
    }
}

fn string_field(message: &DynamicMessage, name: &str) -> String {
    match message.get_field_by_name(name).as_deref() {
        Some(Value::String(value)) => value.trim().to_string(),
        _ => String::new(),
    }
}

fn bytes_field(message: &DynamicMessage, name: &str) -> Vec<u8> {
    match message.get_field_by_name(name).as_deref() {
        Some(Value::Bytes(value)) => value.to_vec(),
        _ => Vec::new(),
    }
}

fn message_field(message: &DynamicMessage, name: &str) -> Option<DynamicMessage> {
    match message.get_field_by_name(name).as_deref() {
        Some(Value::Message(value)) => Some(value.clone()),
        _ => None,
    }
}
