//! Capture summaries, payloads, and frames shown by the original debug UI.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Failures that stop the debug service or its storage.
#[derive(Debug, thiserror::Error)]
#[must_use]
pub enum Error {
    /// The listen address is not a valid loopback address.
    #[error("{0}")]
    Config(String),
    /// SQLite failed; `context` says which operation it was.
    #[error("database error while {context}: {source}")]
    Database {
        /// What the store was doing when SQLite failed.
        context: &'static str,
        /// The underlying SQLite failure.
        #[source]
        source: rusqlite::Error,
    },
    /// A stored or captured payload could not be parsed as JSON.
    #[error("json error while {context}: {source}")]
    Json {
        /// What the service was decoding when the payload failed to parse.
        context: &'static str,
        /// The underlying serializer failure.
        #[source]
        source: serde_json::Error,
    },
    /// The capture database's directory or file could not be prepared.
    #[error("io error while {context}: {source}")]
    Io {
        /// What the store was doing when the filesystem failed.
        context: &'static str,
        /// The underlying IO failure.
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    /// Wraps a SQLite failure with a short description of the failed step.
    pub(crate) fn database(context: &'static str, source: rusqlite::Error) -> Self {
        Self::Database { context, source }
    }

    /// Wraps a JSON failure with a short description of the failed step.
    pub(crate) fn json(context: &'static str, source: serde_json::Error) -> Self {
        Self::Json { context, source }
    }

    /// Wraps an IO failure with a short description of the failed step.
    pub(crate) fn io(context: &'static str, source: std::io::Error) -> Self {
        Self::Io { context, source }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Where an exchange is in its request/response lifecycle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExchangeState {
    /// The request was recorded but no upstream response arrived yet.
    #[default]
    Pending,
    /// The upstream answered and its body is still being captured.
    Streaming,
    /// Both bodies were captured to the end.
    Completed,
    /// The request or its body failed.
    Error,
}

impl ExchangeState {
    /// Whether a stored exchange's bodies are worth re-decoding on load.
    #[must_use]
    pub fn is_replayable(self) -> bool {
        matches!(self, Self::Completed | Self::Streaming)
    }

    /// The persisted column value, matching the `serde` wire name.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Streaming => "streaming",
            Self::Completed => "completed",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeSummary {
    pub id: String,
    pub started_at: DateTime<Utc>,
    pub method: String,
    pub url: String,
    pub host: String,
    pub path: String,
    /// HTTP status; `None` until the upstream answers.
    #[serde(default)]
    pub status: Option<u16>,
    pub state: ExchangeState,
    pub duration_ms: i64,
    pub request_bytes: i64,
    pub response_bytes: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub request_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub request_kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub response_kind: String,
    pub frame_count: i32,
    /// Why the exchange failed, present only for [`ExchangeState::Error`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Exchange {
    #[serde(flatten)]
    pub summary: ExchangeSummary,
    pub request: Payload,
    pub response: Payload,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Payload {
    pub headers: Vec<Header>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content_codec: String,
    pub size: i64,
    /// The captured body as lowercase hex, clipped to what the UI can render.
    /// Empty when nothing was captured.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub raw_hex: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub raw_truncated: bool,
    /// The decoded body, in `decoded_lang`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub decoded_json: String,
    #[serde(
        rename = "decodedLanguage",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub decoded_lang: String,
    /// Why the body could not be decoded. A body that decodes cleanly has
    /// neither this nor [`Payload::raw_hex`] missing for any other reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode_error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frames: Vec<FrameView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameView {
    pub index: i32,
    pub flags: u8,
    pub length: i32,
    pub compressed: bool,
    pub end_stream: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub request_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub json: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub raw_hex: String,
    /// Why this frame alone failed to decode, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl FrameView {
    /// A Connect end-stream frame carries this kind tag.
    pub const KIND_END_STREAM: &str = "end_stream";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSummary {
    pub conversation_id: String,
    pub exchange_count: i32,
    pub last_started_at: DateTime<Utc>,
    pub request_bytes: i64,
    pub response_bytes: i64,
}

/// What changed in the store, as pushed to the debug UI over SSE.
///
/// The wire shape is built in [`crate::web`]; [`StoreEventKind::label`] is the
/// single source of truth for the event name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreEvent {
    pub kind: StoreEventKind,
    /// The exchange that changed, absent for [`StoreEventKind::Cleared`].
    pub id: Option<String>,
}

impl StoreEventKind {
    /// The wire name, matching the SSE payload's `type` field.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Cleared => "cleared",
        }
    }
}

/// The store changes the debug UI subscribes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreEventKind {
    /// A new exchange was recorded.
    Created,
    /// An existing exchange changed.
    Updated,
    /// Every exchange was dropped.
    Cleared,
}
