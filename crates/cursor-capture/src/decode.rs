//! Connect frames, compressed payloads, and Cursor protobuf views.

use std::io::{self, Read};
use std::sync::LazyLock;

use brotli::Decompressor;
use flate2::read::{GzDecoder, ZlibDecoder};
use prost_reflect::{DescriptorPool, DynamicMessage, MethodDescriptor, ReflectMessage, Value};
use serde::Serialize;

use crate::config::{DEFAULT_MAX_FRAMES, MAX_CONNECT_FRAME_BYTES};
use crate::types::{Exchange, FrameView, Payload};

/// How much brotli output the debug view reads per chunk.
const INFLATE_CHUNK_BYTES: usize = 4096;
/// The protobuf messages the capture view unwraps by hand.
const BIDI_REQUEST_ID_MESSAGE: &str = "aiserver.v1.BidiRequestId";
const BIDI_APPEND_MESSAGE: &str = "aiserver.v1.BidiAppendRequest";
const AGENT_CLIENT_MESSAGE: &str = "agent.v1.AgentClientMessage";
/// Connect frame flag bits, from the Connect protocol spec.
const CONNECT_FLAG_COMPRESSED: u8 = 0x01;
const CONNECT_FLAG_END_STREAM: u8 = 0x02;
/// The `[flags u8][length u32 BE]` header preceding every Connect payload.
const CONNECT_ENVELOPE_BYTES: usize = 5;

/// Why a stored or captured payload could not be decoded for display.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The payload was not valid hex.
    #[error("payload is not valid hex while decoding {context}: {source}")]
    Hex {
        /// Which payload was being decoded.
        context: &'static str,
        /// The underlying hex failure.
        #[source]
        source: hex::FromHexError,
    },
    /// A compressed payload could not be inflated.
    #[error("cannot inflate {label}: {source}")]
    Inflate {
        /// Which payload was being inflated.
        label: &'static str,
        /// The underlying IO failure.
        #[source]
        source: std::io::Error,
    },
    /// A protobuf message did not match its descriptor.
    #[error("cannot decode {context} as protobuf: {source}")]
    Protobuf {
        /// Which payload was being decoded.
        context: &'static str,
        /// The underlying protobuf failure.
        #[source]
        source: prost::DecodeError,
    },
    /// The message type is not in the embedded descriptor set.
    #[error("unknown protobuf message type {0}")]
    UnknownType(String),
    /// The content encoding is one this crate does not implement.
    #[error("unsupported content encoding {0}")]
    UnsupportedCodec(String),
    /// The decompressed payload exceeded the frame cap.
    #[error("{label} inflates past the {limit} byte limit")]
    Oversized {
        /// Which payload inflated too far.
        label: &'static str,
        /// The cap it crossed.
        limit: usize,
    },
}

/// One Cursor RPC this debugger knows by name.
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
    DescriptorPool::decode(CURSOR_DESCRIPTORS)
        .unwrap_or_else(|error| panic!("cannot decode the Cursor protobuf descriptors: {error}"))
});

/// Frame and message lengths travel over the wire as `i32`. Cursor never sends
/// a frame past `MAX_CONNECT_FRAME_BYTES`, so anything larger is clamped
/// instead of wrapping into a negative length.
#[must_use]
pub fn clamped_i32(len: usize) -> i32 {
    i32::try_from(len).unwrap_or(i32::MAX)
}

/// A capture limit stored as `i32`, as the `usize` a buffer needs.
#[must_use]
pub fn capture_limit(limit: i32) -> usize {
    usize::try_from(limit.max(0)).unwrap_or(usize::MAX)
}

/// The most decompressed bytes a frame may hold. This is the Cursor Connect
/// frame ceiling, not a policy limit of this crate.
const MAX_DECOMPRESSED_BYTES: usize = MAX_CONNECT_FRAME_BYTES;
/// How much of a frame's payload the debug UI shows as hex before eliding.
const HEX_CLIP_BYTES: usize = 4096;

/// One byte past the decompressed-size cap, so `io::copy` can detect an
/// oversized payload rather than truncating it.
fn frame_limit_bytes() -> u64 {
    u64::try_from(MAX_DECOMPRESSED_BYTES.saturating_add(1)).unwrap_or(u64::MAX)
}

static CURSOR_DESCRIPTORS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cursor.bin"));

static PROTO_JSON: LazyLock<prost_reflect::SerializeOptions> = LazyLock::new(|| {
    prost_reflect::SerializeOptions::new()
        .use_proto_field_name(true)
        .skip_default_fields(true)
});

/// Incrementally turns captured Connect stream bytes into [`FrameView`]s.
///
/// One decoder serves one direction of one exchange: `write` is fed body chunks
/// as they arrive, and each complete frame is handed to the callback given to
/// [`ConnectFrameDecoder::new`]. `close` flushes a trailing partial frame as an
/// error so the UI can show it.
pub struct ConnectFrameDecoder {
    buffer: Vec<u8>,
    message_type: String,
    codec: String,
    max_frames: i32,
    frame_count: i32,
    on_frame: Box<dyn FnMut(FrameView) + Send>,
}

impl std::fmt::Debug for ConnectFrameDecoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectFrameDecoder")
            .field("message_type", &self.message_type)
            .field("codec", &self.codec)
            .field("buffered", &self.buffer.len())
            .field("max_frames", &self.max_frames)
            .field("frame_count", &self.frame_count)
            .field("on_frame", &"<callback>")
            .finish()
    }
}

impl ConnectFrameDecoder {
    /// Builds a decoder for one captured stream direction.
    pub fn new(
        message_type: &str,
        codec: &str,
        max_frames: i32,
        on_frame: impl FnMut(FrameView) + Send + 'static,
    ) -> Self {
        Self {
            buffer: Vec::new(),
            message_type: message_type.to_owned(),
            codec: codec.trim().to_owned(),
            max_frames,
            frame_count: 0,
            on_frame: Box::new(on_frame),
        }
    }

    /// Feeds one chunk of captured bytes through the decoder.
    pub fn write(&mut self, payload: &[u8]) {
        if payload.is_empty() || self.frame_count >= self.max_frames {
            return;
        }
        self.buffer.extend_from_slice(payload);
        // Frames are consumed by index and the buffer is drained once at the
        // end, so a chunk holding many frames does not shift the rest per frame.
        let mut consumed = 0;
        while self.frame_count < self.max_frames {
            let remaining = &self.buffer[consumed..];
            if remaining.len() < CONNECT_ENVELOPE_BYTES {
                break;
            }
            let flags = remaining[0];
            let mut header = [0_u8; 4];
            header.copy_from_slice(&remaining[1..CONNECT_ENVELOPE_BYTES]);
            let length = usize::try_from(u32::from_be_bytes(header)).unwrap_or(usize::MAX);
            if length > MAX_DECOMPRESSED_BYTES {
                self.emit(FrameView {
                    flags,
                    length: clamped_i32(length),
                    error: Some("Connect 帧长度异常".into()),
                    ..FrameView::default()
                });
                self.buffer.clear();
                return;
            }
            if remaining.len() < CONNECT_ENVELOPE_BYTES + length {
                break;
            }
            let frame_payload = &remaining[CONNECT_ENVELOPE_BYTES..CONNECT_ENVELOPE_BYTES + length];
            let frame = self.decode(flags, frame_payload);
            self.emit(frame);
            consumed += CONNECT_ENVELOPE_BYTES + length;
        }
        if consumed > 0 {
            self.buffer.drain(..consumed);
        }
    }

    /// Flushes a truncated trailing frame as an error frame.
    pub fn close(&mut self) {
        if !self.buffer.is_empty() && self.frame_count < self.max_frames {
            self.emit(FrameView {
                length: clamped_i32(self.buffer.len()),
                raw_hex: clipped_hex(&self.buffer, HEX_CLIP_BYTES),
                error: Some("流结束时仍有不完整的 Connect 帧".into()),
                ..FrameView::default()
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
            length: clamped_i32(payload.len()),
            compressed: flags & CONNECT_FLAG_COMPRESSED != 0,
            end_stream: flags & CONNECT_FLAG_END_STREAM != 0,
            raw_hex: clipped_hex(payload, HEX_CLIP_BYTES),
            ..FrameView::default()
        };
        // Only a compressed frame needs owned bytes: an uncompressed frame is
        // decoded straight from the caller's slice, so a normal stream never
        // copies a payload just to share one code path.
        let inflated;
        let decoded: &[u8] = if frame.compressed {
            match decompress_payload(payload, &self.codec) {
                Ok(bytes) => {
                    inflated = bytes;
                    &inflated
                }
                Err(err) => {
                    frame.error = Some(err.to_string());
                    return frame;
                }
            }
        } else {
            payload
        };
        if frame.end_stream {
            frame.kind = FrameView::KIND_END_STREAM.into();
            frame.message_type = "connect.error.v1.EndStreamResponse".into();
            frame.json = pretty_json(decoded);
            return frame;
        }
        match decode_named_message(&self.message_type, decoded) {
            None => frame.error = Some("未知的 protobuf 消息类型".into()),
            Some(Err(err)) => frame.error = Some(format!("protobuf 解码失败：{err}")),
            Some(Ok(message)) => {
                frame.message_type.clone_from(&self.message_type);
                frame.kind = active_oneof_name(&message);
                if self.message_type == BIDI_REQUEST_ID_MESSAGE {
                    frame.request_id = string_field(&message, "request_id");
                }
                frame.json = marshal_proto_json(&message);
            }
        }
        frame
    }
}

/// The result of decoding one unary Connect body: the JSON to show plus the
/// request metadata recovered from it.
#[derive(Debug, Default)]
pub struct DecodedUnary {
    /// The message rendered as protobuf JSON, empty when decoding failed.
    pub json: String,
    /// The RPC kind the path names (`run_sse`, `bidi_append`, …).
    pub kind: String,
    /// The `request_id` the body carried, when the RPC has one.
    pub request_id: String,
    /// The conversation the body belongs to, when it names one.
    pub conversation_id: String,
    /// Why the body could not be decoded, when it could not.
    pub error: Option<String>,
}

/// Decodes one unary request body against the message type its path names.
/// A path this crate knows nothing about decodes as an empty [`DecodedUnary`].
#[must_use]
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
    let request = match decode_named_message(BIDI_APPEND_MESSAGE, payload) {
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
            error: Some(err.to_string()),
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
) -> Result<Option<(DynamicMessage, String)>, DecodeError> {
    let data = string_field(request, "data");
    if !data.trim().is_empty() {
        let payload = hex::decode(data.trim()).map_err(|source| DecodeError::Hex {
            context: "the BidiAppendRequest agent client payload",
            source,
        })?;
        let message = decode_required(AGENT_CLIENT_MESSAGE, &payload)?;
        let kind = active_oneof_name(&message);
        return Ok(Some((message, kind)));
    }
    let binary = bytes_field(request, "data_binary");
    if binary.is_empty() {
        return Ok(None);
    }
    let message = decode_required(AGENT_CLIENT_MESSAGE, &binary)?;
    let kind = active_oneof_name(&message);
    Ok(Some((message, kind)))
}

/// Decodes a message this crate is sure exists in the descriptor set.
fn decode_required(message_type: &str, payload: &[u8]) -> Result<DynamicMessage, DecodeError> {
    let descriptor = POOL
        .get_message_by_name(message_type)
        .ok_or_else(|| DecodeError::UnknownType(message_type.to_owned()))?;
    DynamicMessage::decode(descriptor, payload).map_err(|source| DecodeError::Protobuf {
        context: "a required message",
        source,
    })
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

/// A decoded unary response: its JSON form and the message kind that produced it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DecodedResponse {
    pub json: String,
    pub kind: String,
}

/// Decodes a unary Cursor response into its JSON view.
///
/// # Errors
///
/// Returns [`DecodeError::Protobuf`] when the payload does not match the
/// method's response schema.
pub fn decode_unary_response(path: &str, payload: &[u8]) -> Result<DecodedResponse, DecodeError> {
    let Some((descriptor, kind)) = unary_response_message(path) else {
        return Ok(DecodedResponse::default());
    };
    let message =
        DynamicMessage::decode(descriptor, payload).map_err(|source| DecodeError::Protobuf {
            context: "a unary response",
            source,
        })?;
    Ok(DecodedResponse {
        json: marshal_proto_json(&message),
        kind,
    })
}

/// Whether hydrating a stored exchange changed anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hydration {
    /// At least one body was re-decoded into the stored exchange.
    Changed,
    /// Nothing could be filled in; the stored row is already final.
    Unchanged,
}

impl Hydration {
    /// Whether the exchange needs to be written back.
    #[must_use]
    pub fn is_changed(self) -> bool {
        self == Self::Changed
    }
}

/// The stored bodies the debugger can re-decode: streaming frames, unary
/// protobuf JSON, and plain text/JSON.
#[must_use]
pub fn hydrate_stored_exchange(exchange: &mut Exchange) -> Hydration {
    if !exchange.summary.state.is_replayable() {
        return Hydration::Unchanged;
    }
    let request_frames = hydrate_stored_request_frames(exchange);
    let response_frames = hydrate_stored_response_frames(exchange);
    let request_unary = hydrate_stored_unary_request(exchange);
    let response_unary = hydrate_stored_unary_response(exchange);
    let request_text = hydrate_stored_request_text(exchange);
    let response_text = hydrate_stored_response_text(exchange);
    if request_frames
        || response_frames
        || request_unary
        || response_unary
        || request_text
        || response_text
    {
        Hydration::Changed
    } else {
        Hydration::Unchanged
    }
}

/// Re-decodes one stored frame direction, reporting whether frames were
/// actually filled in (an empty decode is not a change).
fn hydrate_stored_frames(payload: &mut Payload, message_type: Option<String>) -> bool {
    let Some(message_type) = message_type else {
        return false;
    };
    if !payload.frames.is_empty() || payload.raw_hex.is_empty() || payload.raw_truncated {
        return false;
    }
    match decode_stored_connect_frames(&payload.raw_hex, &message_type, &payload.content_codec) {
        Ok(decoded) if !decoded.is_empty() => {
            payload.frames = decoded;
            true
        }
        Ok(_) => false,
        Err(_) => true,
    }
}

fn hydrate_stored_request_frames(exchange: &mut Exchange) -> bool {
    let message_type = streaming_request_message_type(&exchange.summary.path);
    let summary = &mut exchange.summary;
    let payload = &mut exchange.request;
    if !hydrate_stored_frames(payload, message_type) {
        return false;
    }
    if let Some(error) = &payload.decode_error {
        summary.error.get_or_insert_with(|| error.clone());
    }
    for frame in &payload.frames {
        if !frame.kind.is_empty() && frame.kind != FrameView::KIND_END_STREAM {
            summary.request_kind.clone_from(&frame.kind);
        }
        if !frame.request_id.is_empty() {
            summary.request_id.clone_from(&frame.request_id);
        }
    }
    true
}

fn hydrate_stored_response_frames(exchange: &mut Exchange) -> bool {
    let message_type = streaming_response_message_type(&exchange.summary.path);
    let summary = &mut exchange.summary;
    let payload = &mut exchange.response;
    if !hydrate_stored_frames(payload, message_type) {
        return false;
    }
    summary.frame_count = clamped_i32(payload.frames.len());
    for frame in &payload.frames {
        if !frame.kind.is_empty() && frame.kind != FrameView::KIND_END_STREAM {
            summary.response_kind.clone_from(&frame.kind);
        }
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
    let payload =
        match decode_stored_raw_payload(&exchange.request.raw_hex, &exchange.request.content_codec)
        {
            Ok(payload) => payload,
            Err(err) => {
                exchange.request.decode_error = Some(err.to_string());
                return true;
            }
        };
    let decoded = decode_unary_request(&exchange.summary.path, &payload);
    let mut changed = false;
    if let Some(error) = decoded.error {
        exchange.request.decode_error = Some(error);
        changed = true;
    }
    if !decoded.json.is_empty() {
        exchange.request.decoded_json = decoded.json;
        exchange.request.decoded_lang = "json".into();
        changed = true;
    }
    if !decoded.kind.is_empty() {
        exchange.summary.request_kind = decoded.kind;
    }
    if !decoded.request_id.is_empty() {
        exchange.summary.request_id = decoded.request_id;
    }
    if !decoded.conversation_id.is_empty() {
        exchange.summary.conversation_id = decoded.conversation_id;
    }
    changed
}

fn hydrate_stored_unary_response(exchange: &mut Exchange) -> bool {
    if !is_unary_proto_content_type(&exchange.response.content_type)
        || !exchange.response.decoded_json.is_empty()
        || exchange.response.raw_truncated
    {
        return false;
    }
    let payload = match decode_stored_raw_payload(
        &exchange.response.raw_hex,
        &exchange.response.content_codec,
    ) {
        Ok(payload) => payload,
        Err(err) => {
            exchange.response.decode_error = Some(err.to_string());
            return true;
        }
    };
    match decode_unary_response(&exchange.summary.path, &payload) {
        Ok(decoded) if !decoded.json.is_empty() => {
            exchange.response.decoded_json = decoded.json;
            exchange.response.decoded_lang = "json".into();
            exchange.summary.response_kind = decoded.kind;
            true
        }
        Ok(_) => false,
        Err(err) => {
            exchange.response.decode_error = Some(err.to_string());
            true
        }
    }
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
            payload.decode_error = Some(format!("解析已存储正文失败：{err}"));
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
        payload.decode_error = Some(err);
    }
    true
}

/// One body decoded for display: the text, its language for the editor, and
/// why it could not be decoded when it could not.
#[derive(Debug, Default)]
pub struct DecodedContent {
    /// The decoded body as text, empty when decoding failed.
    pub text: String,
    /// The syntax the debug UI highlights `text` with (`json`, `protobuf`, …).
    pub language: String,
    /// Why the body could not be fully decoded, when it could not.
    pub error: Option<String>,
}

/// Decodes one captured body for display, honouring its content type and
/// compression codec. JSON is re-indented; anything else is shown as text.
#[must_use]
pub fn decode_captured_content(payload: &[u8], content_type: &str, codec: &str) -> DecodedContent {
    let decoded = match decode_http_content(payload, codec) {
        Ok(decoded) => decoded,
        Err(err) => {
            return DecodedContent {
                error: Some(err.to_string()),
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
fn inflate_limited(mut reader: impl Read, label: &'static str) -> Result<Vec<u8>, DecodeError> {
    let mut result = Vec::new();
    io::copy(&mut reader.by_ref().take(frame_limit_bytes()), &mut result)
        .map_err(|source| DecodeError::Inflate { label, source })?;
    if result.len() > MAX_DECOMPRESSED_BYTES {
        return Err(DecodeError::Oversized {
            label,
            limit: MAX_DECOMPRESSED_BYTES,
        });
    }
    Ok(result)
}

/// Decompresses `payload` for every encoding named in `codec`, outermost last.
///
/// # Errors
///
/// Returns [`DecodeError::UnsupportedCodec`] for an encoding this crate does
/// not implement, and [`DecodeError::Inflate`]/[`DecodeError::Oversized`] when
/// decompression fails or exceeds the frame cap.
pub fn decode_http_content(payload: &[u8], codec: &str) -> Result<Vec<u8>, DecodeError> {
    let encodings = codec
        .split(',')
        .map(|item| item.trim().to_ascii_lowercase());
    let mut decoded = payload.to_vec();
    for encoding in encodings.rev() {
        match encoding.as_str() {
            "" | "identity" => {}
            "gzip" | "x-gzip" => decoded = decompress_payload(&decoded, "gzip")?,
            "deflate" => {
                let inflater = ZlibDecoder::new(decoded.as_slice());
                decoded = inflate_limited(inflater, "deflate")?;
            }
            "br" => {
                let decompressor = Decompressor::new(decoded.as_slice(), INFLATE_CHUNK_BYTES);
                decoded = inflate_limited(decompressor, "Brotli")?;
            }
            other => return Err(DecodeError::UnsupportedCodec(other.to_owned())),
        }
    }
    Ok(decoded)
}

#[must_use]
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

#[must_use]
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

/// Re-decodes the stored Connect frames of one exchange side.
///
/// # Errors
///
/// Returns [`DecodeError::Hex`] when the stored hex is malformed.
pub fn decode_stored_connect_frames(
    raw_hex_value: &str,
    message_type: &str,
    codec: &str,
) -> Result<Vec<FrameView>, DecodeError> {
    let payload = hex::decode(raw_hex_value.trim()).map_err(|source| DecodeError::Hex {
        context: "a stored Connect body",
        source,
    })?;
    // The callback runs synchronously inside `write`/`close`; a channel keeps the
    // frames reachable after the decoder (which owns the callback) is dropped,
    // without sharing a mutex that could be left poisoned.
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut decoder =
        ConnectFrameDecoder::new(message_type, codec, DEFAULT_MAX_FRAMES, move |frame| {
            // The receiver outlives the decoder, so this never fails.
            let _ = sender.send(frame);
        });
    decoder.write(&payload);
    decoder.close();
    drop(decoder);
    Ok(receiver.try_iter().collect())
}

#[must_use]
pub fn is_unary_proto_content_type(content_type: &str) -> bool {
    matches!(
        normalized_media_type(content_type).as_str(),
        "application/proto" | "application/protobuf" | "application/x-protobuf"
    )
}

/// Decodes a stored payload's hex and content encoding.
///
/// # Errors
///
/// Returns [`DecodeError::Hex`] for malformed hex and the inflate errors for a
/// codec this crate cannot apply.
pub fn decode_stored_raw_payload(raw_hex_value: &str, codec: &str) -> Result<Vec<u8>, DecodeError> {
    let payload = hex::decode(raw_hex_value.trim()).map_err(|source| DecodeError::Hex {
        context: "a stored body",
        source,
    })?;
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

/// The request/response protobuf type a streaming RPC carries, or `None` when
/// `path` is not a streaming method.
#[must_use]
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

/// The response protobuf type a streaming RPC returns, or `None` when `path` is
/// not a streaming method.
#[must_use]
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

/// Whether the debugger knows how to decode this path's request body.
#[must_use]
pub fn decodes_unary_request(path: &str) -> bool {
    path == BIDI_APPEND_PATH || unary_request_message(path).is_some()
}

/// Whether the debugger knows how to decode this path's response body.
#[must_use]
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
    let mut parts = path.trim().trim_matches('/').split('/');
    let (Some(service_name), Some(method_name), None) = (parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    if service_name.is_empty() || method_name.is_empty() {
        return None;
    }
    let service = POOL.get_service_by_name(service_name)?;
    service
        .methods()
        .find(|method| method.name() == method_name)
}

fn proto_message_kind(name: &str) -> String {
    snake_case(name)
}

#[must_use]
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

#[must_use]
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

#[must_use]
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

#[must_use]
pub fn pretty_json(payload: &[u8]) -> String {
    match serde_json::from_slice::<serde_json::Value>(payload) {
        Ok(value) => serde_json::to_string_pretty(&value)
            .unwrap_or_else(|_| String::from_utf8_lossy(payload).into_owned()),
        Err(_) => String::from_utf8_lossy(payload).into_owned(),
    }
}

#[must_use]
pub fn clipped_hex(payload: &[u8], max: usize) -> String {
    if payload.len() > max {
        format!("{}...", hex::encode(&payload[..max]))
    } else {
        hex::encode(payload)
    }
}

#[must_use]
pub fn raw_hex(payload: &[u8]) -> String {
    hex::encode(payload)
}

/// Inflates a gzip payload, rejecting any other encoding.
///
/// # Errors
///
/// Returns [`DecodeError::UnsupportedCodec`] for a non-gzip codec and the
/// inflate errors when the stream is corrupt or too large.
pub fn decompress_payload(payload: &[u8], codec: &str) -> Result<Vec<u8>, DecodeError> {
    if !codec.is_empty() && !codec.eq_ignore_ascii_case("gzip") {
        return Err(DecodeError::UnsupportedCodec(codec.to_owned()));
    }
    inflate_limited(GzDecoder::new(payload), "gzip")
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
