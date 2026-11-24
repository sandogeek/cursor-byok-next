//! Persists provider call payloads, timing, and usage.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use sqlx::{
    Row, SqlitePool,
    sqlite::{
        SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
    },
};
use tokio::sync::{Mutex, OnceCell};

use crate::{Result, model::Usage, saturating, time::now_ms};

/// Oldest calls are dropped past this count, so the table cannot grow forever.
const MAX_RETAINED_CALLS: i64 = 1000;
/// How many of the newest calls the UI list may request at once.
const MAX_LIST_LIMIT: i64 = 500;
const MAX_STORED_REQUEST_BYTES: usize = 1024 * 1024;
/// How long a write waits for Cursor's own SQLite reader before failing.
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Small pool: every write already serializes on `Connected::writes`.
const POOL_MAX_CONNECTIONS: u32 = 8;
/// The call list a snapshot shows when the caller names no limit.
pub const DEFAULT_LIST_LIMIT: usize = 200;

/// Stored response payloads stop at 1 MiB; SQLite stores the total as `i64`.
const MAX_STORED_RESPONSE_BYTES: i64 = 1024 * 1024;

/// The list limit SQLite is asked for, clamped to the UI's range.
fn list_limit(limit: usize) -> i64 {
    let limit = i64::try_from(limit).unwrap_or(MAX_LIST_LIMIT);
    limit.clamp(1, MAX_LIST_LIMIT)
}

const LLM_CALL_COLUMNS: &str = "call_id, run_id, conversation_id, provider_call_index, model_hash,
                provider_type, provider_url, request_type, request_url, model_id, display_name,
                reasoning_effort, fast, status, finish_reason, created_at_ms, request_started_at_ms,
                response_headers_at_ms, first_event_at_ms, first_text_at_ms, first_valid_response_at_ms,
                finished_at_ms, queue_ms, response_headers_ms, ttft_ms, ttfr_ms, duration_ms, input_tokens,
                output_tokens, total_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                usage_json, message_count, tool_count, request_bytes, response_bytes, response_chunk_count,
                http_status, error_kind, error_message, detailed,
                request_truncated, response_truncated";

#[derive(Clone, Debug)]
pub struct NewLlmCall {
    pub call_id: String,
    pub run_id: String,
    pub conversation_id: String,
    pub provider_call_index: i64,
    pub model_hash: String,
    pub provider_type: String,
    pub provider_url: String,
    pub request_type: String,
    pub request_url: String,
    pub model_id: String,
    pub display_name: String,
    pub reasoning_effort: Option<String>,
    pub fast: bool,
    pub message_count: usize,
    pub tool_count: usize,
    pub detailed: bool,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "four independent persisted flags read straight from the calls table; folding them into enums would change the recorded schema for no gain"
)]
#[derive(Clone, Debug, PartialEq)]
pub struct LlmCallSummary {
    pub call_id: String,
    pub run_id: String,
    pub conversation_id: String,
    pub provider_call_index: i64,
    pub model_hash: Option<String>,
    pub provider_type: String,
    pub provider_url: String,
    pub request_type: String,
    pub request_url: String,
    pub model_id: String,
    pub display_name: String,
    pub reasoning_effort: Option<String>,
    pub fast: bool,
    pub status: CallStatus,
    pub finish_reason: Option<String>,
    pub created_at_ms: i64,
    pub request_started_at_ms: Option<i64>,
    pub response_headers_at_ms: Option<i64>,
    pub first_event_at_ms: Option<i64>,
    pub first_text_at_ms: Option<i64>,
    pub first_valid_response_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub queue_ms: Option<i64>,
    pub response_headers_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub ttfr_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub usage: Option<serde_json::Value>,
    pub message_count: i64,
    pub tool_count: i64,
    pub request_bytes: Option<i64>,
    pub response_bytes: i64,
    pub response_chunk_count: i64,
    pub http_status: Option<i64>,
    pub error_kind: Option<String>,
    pub error_message: Option<String>,
    pub detailed: bool,
    pub request_truncated: bool,
    pub response_truncated: bool,
}

/// How a call ended, as `finish_llm_call` records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallOutcome<'a> {
    pub status: CallStatus,
    pub finish_reason: Option<&'a str>,
    /// Time from request start to the terminal state; [`CallRecorder`] stamps
    /// the real value when it finishes the call.
    pub elapsed_ms: i64,
    pub error_kind: Option<&'a str>,
    pub error_message: Option<&'a str>,
}

impl<'a> CallOutcome<'a> {
    /// A call that finished at the provider's own end-of-stream.
    #[must_use]
    pub fn completed(finish_reason: &'a str) -> Self {
        Self {
            status: CallStatus::Completed,
            finish_reason: Some(finish_reason),
            elapsed_ms: 0,
            error_kind: None,
            error_message: None,
        }
    }

    /// A call the client dropped before the stream ended.
    #[must_use]
    pub fn cancelled() -> Self {
        Self {
            status: CallStatus::Cancelled,
            finish_reason: None,
            elapsed_ms: 0,
            error_kind: None,
            error_message: None,
        }
    }

    /// A call that failed; `kind` and `message` are what the call list shows.
    #[must_use]
    pub fn failed(kind: &'a str, message: &'a str) -> Self {
        Self {
            status: CallStatus::Error,
            finish_reason: None,
            elapsed_ms: 0,
            error_kind: Some(kind),
            error_message: Some(message),
        }
    }

    /// Stamps the measured duration, for callers that time the call themselves.
    #[must_use]
    pub fn with_elapsed(mut self, elapsed_ms: i64) -> Self {
        self.elapsed_ms = elapsed_ms;
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LlmCallRequest {
    pub headers: serde_json::Value,
    pub body: serde_json::Value,
    pub byte_count: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LlmCallResponseChunk {
    pub seq: i64,
    pub received_offset_ms: i64,
    pub data: String,
    pub byte_count: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallStorage {
    pub call_count: i64,
    pub request_count: i64,
    pub chunk_count: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CallSnapshot {
    pub calls: Vec<LlmCallSummary>,
    /// Whether payload bodies are being stored.
    pub detailed: bool,
    pub storage: CallStorage,
    pub degraded: bool,
}

/// SQLite has no enum type, so each [`CallStatus`] stores its wire name, which
/// [`CallStatus::as_str`] returns and [`CallStatus::from_stored`] parses back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallStatus {
    /// Accepted and waiting for the provider's first byte.
    Running,
    /// The provider finished the stream normally.
    Completed,
    /// The provider or the request failed.
    Error,
    /// The client dropped the stream before it finished.
    Cancelled,
}

impl CallStatus {
    /// The value stored in `llm_calls.status` and shown by the call list.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parses a stored status. Unknown values read as [`CallStatus::Error`],
    /// because a row this build cannot name also cannot be called finished.
    #[must_use]
    pub fn from_stored(text: &str) -> Self {
        match text {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            _ => Self::Error,
        }
    }
}

impl std::fmt::Display for CallStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Whether a call stores payload bodies or only their metrics.
///
/// The setting lives in `service_settings`; each call snapshots it, because
/// turning the setting off must not retroactively change running calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PayloadStorage {
    /// Timing, byte counts, and token usage only.
    #[default]
    Metrics,
    /// Bodies, headers, and response chunks as well.
    Detailed,
}

impl PayloadStorage {
    pub(crate) fn keeps_payloads(self) -> bool {
        matches!(self, Self::Detailed)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BufferedLlmChunk {
    pub(crate) seq: i64,
    pub(crate) elapsed_ms: i64,
    pub(crate) data: Option<Vec<u8>>,
    pub(crate) byte_count: usize,
}

impl BufferedLlmChunk {
    pub(crate) fn new(seq: i64, elapsed_ms: i64, data: &[u8]) -> Self {
        Self {
            seq,
            elapsed_ms,
            data: Some(data.to_vec()),
            byte_count: data.len(),
        }
    }

    pub(crate) fn metrics(seq: i64, elapsed_ms: i64, byte_count: usize) -> Self {
        Self {
            seq,
            elapsed_ms,
            data: None,
            byte_count,
        }
    }
}

#[derive(Clone)]
#[expect(
    missing_debug_implementations,
    reason = "the store owns a sqlx pool, whose Debug output is not useful here"
)]
pub struct CallStore {
    path: PathBuf,
    connected: Arc<OnceCell<Connected>>,
    degraded: Arc<AtomicBool>,
}

struct Connected {
    pool: SqlitePool,
    writes: Mutex<()>,
}

impl CallStore {
    /// A store over an explicit `calls.db`, connected lazily on first use.
    pub fn at(path: PathBuf) -> Self {
        Self {
            path,
            connected: Arc::new(OnceCell::new()),
            degraded: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Records that a write failed, so the calls screen can say the log is incomplete.
    pub fn mark_degraded(&self) {
        self.degraded.store(true, Ordering::Release);
    }

    /// The calls screen's whole view: recent calls, storage mode, counts and whether the log is degraded. Never fails; an unreadable store reports itself degraded.
    pub async fn snapshot(&self, limit: usize) -> CallSnapshot {
        match self.try_snapshot(limit).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(%error, "cannot read LLM call records");
                self.mark_degraded();
                CallSnapshot {
                    degraded: true,
                    ..CallSnapshot::default()
                }
            }
        }
    }

    async fn try_snapshot(&self, limit: usize) -> Result<CallSnapshot> {
        let calls = self.llm_calls(limit).await?;
        let detailed = self.detailed_logging().await?;
        let storage = self.storage().await?;
        Ok(CallSnapshot {
            calls,
            detailed,
            storage,
            degraded: self.degraded.load(Ordering::Acquire),
        })
    }

    /// Whether payload bodies are being stored for new calls.
    pub async fn detailed_logging(&self) -> Result<bool> {
        let connected = self.connected().await?;
        let value: String = sqlx::query_scalar(
            "SELECT value_json FROM service_settings WHERE setting_key = 'llm_detailed_logging'",
        )
        .fetch_one(&connected.pool)
        .await
        .map_err(|error| self.fail(error))?;
        serde_json::from_str(&value).map_err(Into::into)
    }

    /// The storage mode new calls will snapshot.
    pub async fn payload_storage(&self) -> Result<PayloadStorage> {
        let detailed = self.detailed_logging().await?;
        Ok(if detailed {
            PayloadStorage::Detailed
        } else {
            PayloadStorage::Metrics
        })
    }

    /// Changes the storage mode for calls started from now on; running calls keep theirs.
    pub async fn set_detailed_logging(&self, storage: PayloadStorage) -> Result<()> {
        let enabled = storage.keeps_payloads();
        let connected = self.connected().await?;
        let _write = connected.writes.lock().await;
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES ('llm_detailed_logging', ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(serde_json::to_string(&enabled)?)
        .bind(now_ms())
        .execute(&connected.pool)
        .await
        .map_err(|error| self.fail(error))?;
        Ok(())
    }

    /// Inserts the running row and trims the oldest rows past `MAX_RETAINED_CALLS`.
    pub async fn start_llm_call(&self, call: &NewLlmCall) -> Result<()> {
        let connected = self.connected().await?;
        let _write = connected.writes.lock().await;
        let now = now_ms();
        sqlx::query(
            r"INSERT INTO llm_calls(
                call_id, run_id, conversation_id, provider_call_index, model_hash,
                provider_type, provider_url, request_type, request_url, model_id, display_name,
                reasoning_effort, fast, status,
                created_at_ms, request_started_at_ms, queue_ms, message_count, tool_count, detailed
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'running', ?, ?, 0, ?, ?, ?)",
        )
        .bind(&call.call_id)
        .bind(&call.run_id)
        .bind(&call.conversation_id)
        .bind(call.provider_call_index)
        .bind(&call.model_hash)
        .bind(&call.provider_type)
        .bind(&call.provider_url)
        .bind(&call.request_type)
        .bind(&call.request_url)
        .bind(&call.model_id)
        .bind(&call.display_name)
        .bind(&call.reasoning_effort)
        .bind(call.fast)
        .bind(now)
        .bind(now)
        .bind(saturating::i64_from_usize(call.message_count))
        .bind(saturating::i64_from_usize(call.tool_count))
        .bind(call.detailed)
        .execute(&connected.pool)
        .await
        .map_err(|error| self.fail(error))?;
        sqlx::query(
            "DELETE FROM llm_calls WHERE call_id IN (
                SELECT call_id FROM llm_calls
                ORDER BY created_at_ms DESC, rowid DESC LIMIT -1 OFFSET ?
            )",
        )
        .bind(MAX_RETAINED_CALLS)
        .execute(&connected.pool)
        .await
        .map_err(|error| self.fail(error))?;
        Ok(())
    }

    /// Stores the request body (and headers, in detailed mode) and its byte count.
    pub async fn record_llm_request(
        &self,
        call_id: &str,
        headers: &serde_json::Value,
        body: &serde_json::Value,
        storage: PayloadStorage,
    ) -> Result<()> {
        let detailed = storage.keeps_payloads();
        let body_json = serde_json::to_string(body)?;
        // `body_json` is the stored text in the short branch, so read its
        // length once instead of cloning it to keep the original around.
        let body_len = body_json.len();
        let truncated = body_len > MAX_STORED_REQUEST_BYTES;
        let stored_body = if truncated {
            serde_json::json!({
                "truncated": true,
                "originalBytes": body_len,
            })
            .to_string()
        } else {
            body_json
        };
        let headers_json = detailed
            .then(|| serde_json::to_string(headers))
            .transpose()?;
        let connected = self.connected().await?;
        let _write = connected.writes.lock().await;
        let mut transaction = connected
            .pool
            .begin()
            .await
            .map_err(|error| self.fail(error))?;
        if detailed {
            sqlx::query("INSERT INTO llm_call_requests(call_id, headers_json, body_json, byte_count) SELECT ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM llm_calls WHERE call_id = ? AND status = 'running')")
                .bind(call_id)
                .bind(headers_json)
                .bind(&stored_body)
                .bind(saturating::i64_from_usize(body_len))
                .bind(call_id)
                .execute(&mut *transaction)
                .await
                .map_err(|error| self.fail(error))?;
        }
        sqlx::query("UPDATE llm_calls SET request_bytes = ?, request_truncated = ? WHERE call_id = ? AND status = 'running'")
            .bind(saturating::i64_from_usize(body_len))
            .bind(truncated)
            .bind(call_id)
            .execute(&mut *transaction)
            .await
            .map_err(|error| self.fail(error))?;
        transaction
            .commit()
            .await
            .map_err(|error| self.fail(error))?;
        Ok(())
    }

    /// Stamps the upstream status and the time to response headers.
    pub async fn record_llm_response_headers(
        &self,
        call_id: &str,
        elapsed_ms: i64,
        http_status: u16,
    ) -> Result<()> {
        let connected = self.connected().await?;
        let _write = connected.writes.lock().await;
        sqlx::query("UPDATE llm_calls SET response_headers_at_ms = COALESCE(response_headers_at_ms, ?), response_headers_ms = COALESCE(response_headers_ms, ?), http_status = ? WHERE call_id = ? AND status = 'running'")
            .bind(now_ms())
            .bind(elapsed_ms)
            .bind(i64::from(http_status))
            .bind(call_id)
            .execute(&connected.pool)
            .await
            .map_err(|error| self.fail(error))?;
        Ok(())
    }

    pub(crate) async fn record_llm_chunks(
        &self,
        call_id: &str,
        chunks: &[BufferedLlmChunk],
        storage: PayloadStorage,
        first_event_at_ms: i64,
    ) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let detailed = storage.keeps_payloads();
        let byte_count = chunks
            .iter()
            .map(|chunk| saturating::i64_from_usize(chunk.byte_count))
            .sum::<i64>();
        let chunk_count = saturating::i64_from_usize(chunks.len());
        let connected = self.connected().await?;
        let _write = connected.writes.lock().await;
        let mut transaction = connected
            .pool
            .begin()
            .await
            .map_err(|error| self.fail(error))?;
        if detailed {
            let mut stored_bytes = sqlx::query_scalar::<_, i64>(
                "SELECT COALESCE(SUM(byte_count), 0) FROM llm_call_response_chunks WHERE call_id = ?",
            )
            .bind(call_id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(|error| self.fail(error))?;
            let mut truncated = false;
            for chunk in chunks {
                let data = chunk.data.as_deref().ok_or_else(|| {
                    crate::Error::Storage(format!(
                        "detailed chunk {} of call {call_id} has no payload data",
                        chunk.seq
                    ))
                })?;
                let remaining = usize::try_from((MAX_STORED_RESPONSE_BYTES - stored_bytes).max(0))
                    .unwrap_or(usize::MAX);
                if remaining == 0 || data.len() > remaining {
                    truncated = true;
                    continue;
                }
                sqlx::query("INSERT INTO llm_call_response_chunks(call_id, seq, received_offset_ms, data, byte_count) SELECT ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM llm_calls WHERE call_id = ? AND status = 'running')")
                    .bind(call_id)
                    .bind(chunk.seq)
                    .bind(chunk.elapsed_ms)
                    .bind(data)
                    .bind(saturating::i64_from_usize(chunk.byte_count))
                    .bind(call_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| self.fail(error))?;
                stored_bytes += saturating::i64_from_usize(chunk.byte_count);
            }
            if truncated || stored_bytes >= MAX_STORED_RESPONSE_BYTES {
                sqlx::query("UPDATE llm_calls SET response_truncated = 1 WHERE call_id = ? AND status = 'running'")
                    .bind(call_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| self.fail(error))?;
            }
        }
        sqlx::query("UPDATE llm_calls SET first_event_at_ms = COALESCE(first_event_at_ms, ?), response_bytes = response_bytes + ?, response_chunk_count = response_chunk_count + ? WHERE call_id = ? AND status = 'running'")
            .bind(first_event_at_ms)
            .bind(byte_count)
            .bind(chunk_count)
            .bind(call_id)
            .execute(&mut *transaction)
            .await
            .map_err(|error| self.fail(error))?;
        transaction
            .commit()
            .await
            .map_err(|error| self.fail(error))?;
        Ok(())
    }

    /// Stamps the time to the first response event that carries content.
    pub async fn record_llm_first_valid_response(
        &self,
        call_id: &str,
        elapsed_ms: i64,
    ) -> Result<()> {
        let connected = self.connected().await?;
        let _write = connected.writes.lock().await;
        sqlx::query("UPDATE llm_calls SET first_valid_response_at_ms = COALESCE(first_valid_response_at_ms, ?), ttfr_ms = COALESCE(ttfr_ms, ?) WHERE call_id = ? AND status = 'running'")
            .bind(now_ms())
            .bind(elapsed_ms)
            .bind(call_id)
            .execute(&connected.pool)
            .await
            .map_err(|error| self.fail(error))?;
        Ok(())
    }

    /// Stamps the time to first non-empty text delta.
    pub async fn record_llm_first_text(&self, call_id: &str, elapsed_ms: i64) -> Result<()> {
        let connected = self.connected().await?;
        let _write = connected.writes.lock().await;
        sqlx::query("UPDATE llm_calls SET first_text_at_ms = COALESCE(first_text_at_ms, ?), ttft_ms = COALESCE(ttft_ms, ?) WHERE call_id = ? AND status = 'running'")
            .bind(now_ms())
            .bind(elapsed_ms)
            .bind(call_id)
            .execute(&connected.pool)
            .await
            .map_err(|error| self.fail(error))?;
        Ok(())
    }

    /// Stores the provider's token usage.
    pub async fn record_llm_usage(&self, call_id: &str, usage: Usage) -> Result<()> {
        let usage_json = serde_json::to_string(&usage)?;
        let connected = self.connected().await?;
        let _write = connected.writes.lock().await;
        sqlx::query("UPDATE llm_calls SET input_tokens = ?, output_tokens = ?, total_tokens = ?, cache_read_tokens = ?, cache_write_tokens = ?, reasoning_tokens = ?, usage_json = ? WHERE call_id = ? AND status = 'running'")
            .bind(saturating::option_i64_from_u64(usage.input_tokens))
            .bind(saturating::option_i64_from_u64(usage.output_tokens))
            .bind(saturating::option_i64_from_u64(usage.total_tokens))
            .bind(saturating::option_i64_from_u64(usage.cache_read_tokens))
            .bind(saturating::option_i64_from_u64(usage.cache_write_tokens))
            .bind(saturating::option_i64_from_u64(usage.reasoning_tokens))
            .bind(usage_json)
            .bind(call_id)
            .execute(&connected.pool)
            .await
            .map_err(|error| self.fail(error))?;
        Ok(())
    }

    /// Marks a running call finished. A call that already finished is left
    /// alone, which is what makes a late `failed` after `completed` harmless.
    pub async fn finish_llm_call(&self, call_id: &str, outcome: CallOutcome<'_>) -> Result<()> {
        let CallOutcome {
            status,
            finish_reason,
            elapsed_ms,
            error_kind,
            error_message,
        } = outcome;
        let connected = self.connected().await?;
        let _write = connected.writes.lock().await;
        sqlx::query("UPDATE llm_calls SET status = ?, finish_reason = ?, finished_at_ms = ?, duration_ms = ?, error_kind = ?, error_message = ? WHERE call_id = ? AND status = 'running'")
            .bind(status.as_str())
            .bind(finish_reason)
            .bind(now_ms())
            .bind(elapsed_ms)
            .bind(error_kind)
            .bind(error_message)
            .bind(call_id)
            .execute(&connected.pool)
            .await
            .map_err(|error| self.fail(error))?;
        Ok(())
    }

    /// The most recent `limit` calls, newest first.
    pub async fn llm_calls(&self, limit: usize) -> Result<Vec<LlmCallSummary>> {
        let connected = self.connected().await?;
        let rows = sqlx::query(&format!(
            "SELECT {LLM_CALL_COLUMNS} FROM llm_calls ORDER BY created_at_ms DESC, rowid DESC LIMIT ?"
        ))
        .bind(list_limit(limit))
        .fetch_all(&connected.pool)
        .await
        .map_err(|error| self.fail(error))?;
        rows.iter().map(summary_from_row).collect()
    }

    /// One call summary, or `None` when no row matches.
    pub async fn llm_call(&self, call_id: &str) -> Result<Option<LlmCallSummary>> {
        let connected = self.connected().await?;
        sqlx::query(&format!(
            "SELECT {LLM_CALL_COLUMNS} FROM llm_calls WHERE call_id = ?"
        ))
        .bind(call_id)
        .fetch_optional(&connected.pool)
        .await
        .map_err(|error| self.fail(error))?
        .as_ref()
        .map(summary_from_row)
        .transpose()
    }

    /// The stored request, present only for a detailed call.
    pub async fn llm_call_request(&self, call_id: &str) -> Result<Option<LlmCallRequest>> {
        let connected = self.connected().await?;
        let row = sqlx::query(
            "SELECT headers_json, body_json, byte_count FROM llm_call_requests WHERE call_id = ?",
        )
        .bind(call_id)
        .fetch_optional(&connected.pool)
        .await
        .map_err(|error| self.fail(error))?;
        row.map(|row| {
            Ok(LlmCallRequest {
                headers: serde_json::from_str(row.try_get("headers_json")?)?,
                body: serde_json::from_str(row.try_get("body_json")?)?,
                byte_count: row.try_get("byte_count")?,
            })
        })
        .transpose()
    }

    /// The stored response chunks, present only for a detailed call.
    pub async fn llm_call_chunks(&self, call_id: &str) -> Result<Vec<LlmCallResponseChunk>> {
        let connected = self.connected().await?;
        let rows = sqlx::query("SELECT seq, received_offset_ms, data, byte_count FROM llm_call_response_chunks WHERE call_id = ? ORDER BY seq")
            .bind(call_id)
            .fetch_all(&connected.pool)
            .await
            .map_err(|error| self.fail(error))?;
        rows.into_iter()
            .map(|row| {
                Ok(LlmCallResponseChunk {
                    seq: row.try_get("seq")?,
                    received_offset_ms: row.try_get("received_offset_ms")?,
                    data: String::from_utf8_lossy(&row.try_get::<Vec<u8>, _>("data")?).into_owned(),
                    byte_count: row.try_get("byte_count")?,
                })
            })
            .collect()
    }

    /// How many calls, requests and chunks the database holds.
    pub async fn storage(&self) -> Result<CallStorage> {
        let connected = self.connected().await?;
        let (call_count, request_count, chunk_count) = sqlx::query_as::<_, (i64, i64, i64)>(
            "SELECT
                (SELECT COUNT(*) FROM llm_calls),
                (SELECT COUNT(*) FROM llm_call_requests),
                (SELECT COUNT(*) FROM llm_call_response_chunks)",
        )
        .fetch_one(&connected.pool)
        .await
        .map_err(|error| self.fail(error))?;
        Ok(CallStorage {
            call_count,
            request_count,
            chunk_count,
        })
    }

    /// Deletes stored payloads and keeps the summaries.
    pub async fn clear_details(&self) -> Result<CallSnapshot> {
        let connected = self.connected().await?;
        let write_guard = connected.writes.lock().await;
        let mut transaction = connected
            .pool
            .begin()
            .await
            .map_err(|error| self.fail(error))?;
        sqlx::query("DELETE FROM llm_call_requests")
            .execute(&mut *transaction)
            .await
            .map_err(|error| self.fail(error))?;
        sqlx::query("DELETE FROM llm_call_response_chunks")
            .execute(&mut *transaction)
            .await
            .map_err(|error| self.fail(error))?;
        sqlx::query("UPDATE llm_calls SET request_truncated = 0, response_truncated = 0")
            .execute(&mut *transaction)
            .await
            .map_err(|error| self.fail(error))?;
        transaction
            .commit()
            .await
            .map_err(|error| self.fail(error))?;
        drop(write_guard);
        self.try_snapshot(DEFAULT_LIST_LIMIT).await
    }

    /// Deletes every recorded call.
    pub async fn clear_all(&self) -> Result<CallSnapshot> {
        let connected = self.connected().await?;
        let write_guard = connected.writes.lock().await;
        let mut transaction = connected
            .pool
            .begin()
            .await
            .map_err(|error| self.fail(error))?;
        sqlx::query("DELETE FROM llm_calls")
            .execute(&mut *transaction)
            .await
            .map_err(|error| self.fail(error))?;
        transaction
            .commit()
            .await
            .map_err(|error| self.fail(error))?;
        drop(write_guard);
        self.try_snapshot(DEFAULT_LIST_LIMIT).await
    }

    async fn connected(&self) -> Result<&Connected> {
        self.connected.get_or_try_init(|| connect(&self.path)).await
    }

    fn fail(&self, error: sqlx::Error) -> crate::Error {
        self.mark_degraded();
        crate::Error::from(error)
    }
}

async fn connect(path: &Path) -> Result<Connected> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .busy_timeout(SQLITE_BUSY_TIMEOUT);
    let pool = SqlitePoolOptions::new()
        .max_connections(POOL_MAX_CONNECTIONS)
        .connect_with(options)
        .await?;
    apply_schema(&pool).await?;
    Ok(Connected {
        pool,
        writes: Mutex::new(()),
    })
}

async fn apply_schema(pool: &SqlitePool) -> Result<()> {
    let mut transaction = pool.begin().await?;
    for statement in include_str!("../../migrations/0001_calls.sql")
        .split(';')
        .map(str::trim)
        .filter(|sql| !sql.is_empty())
    {
        sqlx::query(statement).execute(&mut *transaction).await?;
    }
    transaction.commit().await?;
    Ok(())
}

fn summary_from_row(row: &SqliteRow) -> Result<LlmCallSummary> {
    let usage = row.try_get::<Option<String>, _>("usage_json")?;
    Ok(LlmCallSummary {
        call_id: row.try_get("call_id")?,
        run_id: row.try_get("run_id")?,
        conversation_id: row.try_get("conversation_id")?,
        provider_call_index: row.try_get("provider_call_index")?,
        model_hash: row.try_get("model_hash")?,
        provider_type: row.try_get("provider_type")?,
        provider_url: row.try_get("provider_url")?,
        request_type: row.try_get("request_type")?,
        request_url: row.try_get("request_url")?,
        model_id: row.try_get("model_id")?,
        display_name: row.try_get("display_name")?,
        reasoning_effort: row.try_get("reasoning_effort")?,
        fast: row.try_get("fast")?,
        status: CallStatus::from_stored(&row.try_get::<String, _>("status")?),
        finish_reason: row.try_get("finish_reason")?,
        created_at_ms: row.try_get("created_at_ms")?,
        request_started_at_ms: row.try_get("request_started_at_ms")?,
        response_headers_at_ms: row.try_get("response_headers_at_ms")?,
        first_event_at_ms: row.try_get("first_event_at_ms")?,
        first_text_at_ms: row.try_get("first_text_at_ms")?,
        first_valid_response_at_ms: row.try_get("first_valid_response_at_ms")?,
        finished_at_ms: row.try_get("finished_at_ms")?,
        queue_ms: row.try_get("queue_ms")?,
        response_headers_ms: row.try_get("response_headers_ms")?,
        ttft_ms: row.try_get("ttft_ms")?,
        ttfr_ms: row.try_get("ttfr_ms")?,
        duration_ms: row.try_get("duration_ms")?,
        input_tokens: row.try_get("input_tokens")?,
        output_tokens: row.try_get("output_tokens")?,
        total_tokens: row.try_get("total_tokens")?,
        cache_read_tokens: row.try_get("cache_read_tokens")?,
        cache_write_tokens: row.try_get("cache_write_tokens")?,
        reasoning_tokens: row.try_get("reasoning_tokens")?,
        usage: usage
            .map(|value| serde_json::from_str(&value))
            .transpose()?,
        message_count: row.try_get("message_count")?,
        tool_count: row.try_get("tool_count")?,
        request_bytes: row.try_get("request_bytes")?,
        response_bytes: row.try_get("response_bytes")?,
        response_chunk_count: row.try_get("response_chunk_count")?,
        http_status: row.try_get("http_status")?,
        error_kind: row.try_get("error_kind")?,
        error_message: row.try_get("error_message")?,
        detailed: row.try_get("detailed")?,
        request_truncated: row.try_get("request_truncated")?,
        response_truncated: row.try_get("response_truncated")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(call_id: &str, model_id: &str, storage: PayloadStorage) -> NewLlmCall {
        NewLlmCall {
            call_id: call_id.into(),
            run_id: format!("run-{call_id}"),
            conversation_id: "conversation".into(),
            provider_call_index: 0,
            model_hash: format!("plugin:test/provider/{model_id}"),
            provider_type: "plugin".into(),
            provider_url: "plugin://test/provider".into(),
            request_type: "plugin".into(),
            request_url: "plugin://test/provider".into(),
            model_id: model_id.into(),
            display_name: model_id.into(),
            reasoning_effort: None,
            fast: false,
            message_count: 1,
            tool_count: 0,
            detailed: storage.keeps_payloads(),
        }
    }

    #[tokio::test]
    async fn a_fresh_database_applies_migrations_and_records_plugin_calls() {
        let directory = tempfile::tempdir().unwrap();
        let store = CallStore::at(directory.path().join("calls.db"));
        store
            .start_llm_call(&sample("plugin-call", "gpt-test", PayloadStorage::Metrics))
            .await
            .unwrap();
        store
            .finish_llm_call(
                "plugin-call",
                CallOutcome::completed("stop").with_elapsed(10),
            )
            .await
            .unwrap();
        let call = store.llm_call("plugin-call").await.unwrap().unwrap();
        assert_eq!(call.status, CallStatus::Completed);
        assert_eq!(call.finish_reason.as_deref(), Some("stop"));
        assert_eq!(
            call.model_hash.as_deref(),
            Some("plugin:test/provider/gpt-test")
        );
        assert!(!call.detailed);
    }

    #[tokio::test]
    async fn detailed_mode_is_snapshotted_per_call_and_clears_payloads_separately() {
        let directory = tempfile::tempdir().unwrap();
        let store = CallStore::at(directory.path().join("calls.db"));
        store
            .set_detailed_logging(PayloadStorage::Detailed)
            .await
            .unwrap();
        let mut call = sample("detailed", "model", PayloadStorage::Detailed);
        call.detailed = store.detailed_logging().await.unwrap();
        store.start_llm_call(&call).await.unwrap();
        store
            .record_llm_request(
                "detailed",
                &serde_json::json!({"content-type": "application/json"}),
                &serde_json::json!({"model": "test"}),
                PayloadStorage::Detailed,
            )
            .await
            .unwrap();
        store
            .record_llm_chunks(
                "detailed",
                &[BufferedLlmChunk::new(0, 1, b"data: [DONE]\n\n")],
                PayloadStorage::Detailed,
                now_ms(),
            )
            .await
            .unwrap();
        store
            .set_detailed_logging(PayloadStorage::Metrics)
            .await
            .unwrap();
        assert!(!store.detailed_logging().await.unwrap());
        assert!(store.llm_call("detailed").await.unwrap().unwrap().detailed);
        assert_eq!(store.storage().await.unwrap().request_count, 1);
        let snapshot = store.clear_details().await.unwrap();
        assert_eq!(snapshot.storage.call_count, 1);
        assert_eq!(snapshot.storage.request_count, 0);
        assert_eq!(snapshot.storage.chunk_count, 0);
        assert!(store.llm_call_request("detailed").await.unwrap().is_none());
        let snapshot = store.clear_all().await.unwrap();
        assert_eq!(snapshot.storage.call_count, 0);
        assert!(store.llm_call("detailed").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn timing_and_finish_are_first_wins_and_running_only() {
        let directory = tempfile::tempdir().unwrap();
        let store = CallStore::at(directory.path().join("calls.db"));
        store
            .start_llm_call(&sample("timing", "model", PayloadStorage::Metrics))
            .await
            .unwrap();
        store
            .record_llm_first_valid_response("timing", 12)
            .await
            .unwrap();
        store
            .record_llm_first_valid_response("timing", 40)
            .await
            .unwrap();
        store.record_llm_first_text("timing", 18).await.unwrap();
        store.record_llm_first_text("timing", 50).await.unwrap();
        store
            .finish_llm_call("timing", CallOutcome::cancelled().with_elapsed(20))
            .await
            .unwrap();
        store
            .record_llm_usage(
                "timing",
                Usage {
                    input_tokens: Some(99),
                    output_tokens: Some(99),
                    total_tokens: Some(198),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        store
            .finish_llm_call("timing", CallOutcome::completed("stop").with_elapsed(99))
            .await
            .unwrap();
        let call = store.llm_call("timing").await.unwrap().unwrap();
        assert_eq!(call.ttfr_ms, Some(12));
        assert_eq!(call.ttft_ms, Some(18));
        assert_eq!(call.input_tokens, None);
        assert_eq!(call.status, CallStatus::Cancelled);
        assert_eq!(call.duration_ms, Some(20));
    }

    #[tokio::test]
    async fn lists_newest_first_and_clamps_the_limit() {
        let directory = tempfile::tempdir().unwrap();
        let store = CallStore::at(directory.path().join("calls.db"));
        store
            .start_llm_call(&sample("a", "a", PayloadStorage::Metrics))
            .await
            .unwrap();
        store
            .start_llm_call(&sample("b", "b", PayloadStorage::Metrics))
            .await
            .unwrap();
        let calls = store.llm_calls(200).await.unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].call_id, "b");
        assert_eq!(calls[1].call_id, "a");
        assert_eq!(store.llm_calls(0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn usage_round_trips_through_json_and_columns() {
        let directory = tempfile::tempdir().unwrap();
        let store = CallStore::at(directory.path().join("calls.db"));
        store
            .start_llm_call(&sample("usage", "model", PayloadStorage::Metrics))
            .await
            .unwrap();
        store
            .record_llm_usage(
                "usage",
                Usage {
                    input_tokens: Some(10),
                    context_input_tokens: Some(12),
                    output_tokens: Some(4),
                    total_tokens: Some(14),
                    cache_read_tokens: Some(2),
                    cache_write_tokens: Some(1),
                    reasoning_tokens: Some(3),
                },
            )
            .await
            .unwrap();
        let call = store.llm_call("usage").await.unwrap().unwrap();
        assert_eq!(call.input_tokens, Some(10));
        assert_eq!(call.cache_read_tokens, Some(2));
        assert_eq!(call.usage.unwrap()["context_input_tokens"], 12);
    }

    #[tokio::test]
    async fn finished_calls_ignore_late_lifecycle_writes() {
        let directory = tempfile::tempdir().unwrap();
        let store = CallStore::at(directory.path().join("calls.db"));
        store
            .start_llm_call(&sample("finished", "model", PayloadStorage::Detailed))
            .await
            .unwrap();
        store
            .finish_llm_call("finished", CallOutcome::cancelled().with_elapsed(20))
            .await
            .unwrap();
        let before = store.llm_call("finished").await.unwrap().unwrap();

        store
            .record_llm_response_headers("finished", 99, 500)
            .await
            .unwrap();
        store
            .record_llm_first_valid_response("finished", 99)
            .await
            .unwrap();
        store.record_llm_first_text("finished", 99).await.unwrap();
        store
            .record_llm_usage(
                "finished",
                Usage {
                    input_tokens: Some(99),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        store
            .record_llm_chunks(
                "finished",
                &[BufferedLlmChunk::new(0, 99, b"late")],
                PayloadStorage::Detailed,
                now_ms(),
            )
            .await
            .unwrap();
        store
            .record_llm_request(
                "finished",
                &serde_json::json!({}),
                &serde_json::json!({"late": true}),
                PayloadStorage::Detailed,
            )
            .await
            .unwrap();

        assert_eq!(store.llm_call("finished").await.unwrap().unwrap(), before);
        assert_eq!(store.storage().await.unwrap().chunk_count, 0);
        assert_eq!(store.storage().await.unwrap().request_count, 0);
    }

    #[tokio::test]
    async fn detailed_payloads_are_bounded_and_marked_truncated() {
        let directory = tempfile::tempdir().unwrap();
        let store = CallStore::at(directory.path().join("calls.db"));
        store
            .start_llm_call(&sample("bounded", "model", PayloadStorage::Detailed))
            .await
            .unwrap();
        store
            .record_llm_request(
                "bounded",
                &serde_json::json!({"content-type": "application/json"}),
                &serde_json::json!({"body": "x".repeat(MAX_STORED_REQUEST_BYTES + 1)}),
                PayloadStorage::Detailed,
            )
            .await
            .unwrap();
        store
            .record_llm_chunks(
                "bounded",
                &[
                    BufferedLlmChunk::new(0, 1, &payload_at_limit()),
                    BufferedLlmChunk::new(1, 2, b"discarded"),
                ],
                PayloadStorage::Detailed,
                now_ms(),
            )
            .await
            .unwrap();

        store
            .record_llm_chunks(
                "bounded",
                &[BufferedLlmChunk::new(2, 3, b"after-limit")],
                PayloadStorage::Detailed,
                now_ms(),
            )
            .await
            .unwrap();
        let call = store.llm_call("bounded").await.unwrap().unwrap();
        assert!(call.request_truncated);
        assert!(call.response_truncated);
        assert_eq!(call.response_bytes, MAX_STORED_RESPONSE_BYTES + 20);
        let request = store.llm_call_request("bounded").await.unwrap().unwrap();
        assert_eq!(request.body["truncated"], true);
        let chunks = store.llm_call_chunks("bounded").await.unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].byte_count, MAX_STORED_RESPONSE_BYTES);
        let cleared = store.clear_details().await.unwrap();
        assert!(!cleared.calls[0].request_truncated);
        assert!(!cleared.calls[0].response_truncated);
    }

    fn payload_at_limit() -> Vec<u8> {
        vec![b'a'; usize::try_from(MAX_STORED_RESPONSE_BYTES).expect("cap fits in usize")]
    }

    #[tokio::test]
    async fn retention_keeps_the_newest_thousand_call_summaries() {
        let directory = tempfile::tempdir().unwrap();
        let store = CallStore::at(directory.path().join("calls.db"));
        for index in 0..=MAX_RETAINED_CALLS {
            store
                .start_llm_call(&sample(
                    &format!("call-{index}"),
                    "model",
                    PayloadStorage::Metrics,
                ))
                .await
                .unwrap();
        }
        let calls = store.llm_calls(500).await.unwrap();
        assert_eq!(
            store.storage().await.unwrap().call_count,
            MAX_RETAINED_CALLS
        );
        assert_eq!(calls[0].call_id, format!("call-{MAX_RETAINED_CALLS}"));
        assert!(store.llm_call("call-0").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_failed_read_returns_a_degraded_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing").join("calls.db");
        let store = CallStore::at(path.clone());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not a database").unwrap();
        let snapshot = store.snapshot(20).await;
        assert!(snapshot.degraded);
        assert_eq!(snapshot.calls, Vec::<LlmCallSummary>::new());
    }
}
