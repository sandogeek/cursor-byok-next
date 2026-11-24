//! Shared errors for the local loop backend.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything the local loop can fail with, plus how it maps onto HTTP.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Protocol(String),
    #[error("{0}")]
    Provider(String),
    #[error("run was cancelled")]
    Cancelled,
    #[error("{0}")]
    NotFound(String),
    /// A plugin runtime (the Bun worker or its executable) is unavailable.
    #[error("{0}")]
    Runtime(String),
    /// A storage operation failed and the message is all there is to say.
    #[error("{0}")]
    Storage(String),
    /// A provider answered with a non-2xx status.
    #[error("HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error(transparent)]
    Decode(#[from] prost::DecodeError),
    #[error(transparent)]
    Encode(#[from] prost::EncodeError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    /// SQLite failed; `context` says which operation it was.
    #[error("storage error while {context}: {source}")]
    Database {
        context: &'static str,
        #[source]
        source: sqlx::Error,
    },
    /// An operation failed for a reason the caller can only log; `context` says
    /// which operation, and the cause keeps its source chain.
    #[error("{context}: {source}")]
    Context {
        context: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Cursor takeover failed; the cause is kept for the log line.
    #[error(transparent)]
    Harness(#[from] crate::harness::Error),
}

impl Error {
    /// Wraps a foreign failure with the operation that produced it, keeping the
    /// cause reachable through [`std::error::Error::source`].
    pub(crate) fn context(
        context: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Context {
            context: context.into(),
            source: Box::new(source),
        }
    }
}

impl From<sqlx::Error> for Error {
    fn from(source: sqlx::Error) -> Self {
        Self::Database {
            context: "running a call-record query",
            source,
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Config(_) | Self::Protocol(_) | Self::Decode(_) | Self::Json(_) => {
                StatusCode::BAD_REQUEST
            }
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Provider(_) | Self::Http(_) | Self::HttpStatus { .. } => StatusCode::BAD_GATEWAY,
            Self::Cancelled => StatusCode::CONFLICT,
            Self::Encode(_)
            | Self::Io(_)
            | Self::Storage(_)
            | Self::Database { .. }
            | Self::Context { .. }
            | Self::Harness(_)
            | Self::Runtime(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        tracing::warn!(%status, error = %self, "local loop request failed");
        let code = match status {
            StatusCode::BAD_REQUEST => "invalid_argument",
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::CONFLICT => "aborted",
            StatusCode::BAD_GATEWAY => "unavailable",
            _ => "internal",
        };
        (
            status,
            Json(serde_json::json!({ "code": code, "message": self.to_string() })),
        )
            .into_response()
    }
}
