//! Capture summaries, payloads, and frames shown by the original debug UI.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Database(String),
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    Upstream(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeSummary {
    pub id: String,
    pub started_at: DateTime<Utc>,
    pub method: String,
    pub url: String,
    pub host: String,
    pub path: String,
    pub status: i32,
    pub state: String,
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Exchange {
    #[serde(flatten)]
    pub summary: ExchangeSummary,
    pub request: Payload,
    pub response: Payload,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Payload {
    pub headers: Vec<Header>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content_codec: String,
    pub size: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub raw_hex: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub raw_truncated: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub decoded_json: String,
    #[serde(
        rename = "decodedLanguage",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub decoded_lang: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub decode_error: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frames: Vec<FrameView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSummary {
    pub conversation_id: String,
    pub exchange_count: i32,
    pub last_started_at: DateTime<Utc>,
    pub request_bytes: i64,
    pub response_bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreEvent {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl Exchange {
    #[must_use]
    pub fn clone_exchange(&self) -> Self {
        self.clone()
    }
}
