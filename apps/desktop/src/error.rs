//! Shared errors for the local loop backend.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};

pub type Result<T, E = Error> = std::result::Result<T, E>;

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
    #[error("{0}")]
    Storage(String),
}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

impl From<crate::plugins::Error> for Error {
    fn from(error: crate::plugins::Error) -> Self {
        match error {
            crate::plugins::Error::Config(message) => Self::Config(message),
            crate::plugins::Error::Protocol(message) => Self::Protocol(message),
            crate::plugins::Error::Provider(message) => Self::Provider(message),
            crate::plugins::Error::NotFound(message) => Self::NotFound(message),
            crate::plugins::Error::HttpStatus { status, body } => {
                Self::Provider(format!("HTTP {status}: {body}"))
            }
            crate::plugins::Error::Cancelled => Self::Cancelled,
            crate::plugins::Error::Json(error) => Self::Json(error),
            crate::plugins::Error::Io(error) => Self::Io(error),
            crate::plugins::Error::Runtime(message) | crate::plugins::Error::Storage(message) => {
                Self::Storage(message)
            }
        }
    }
}

impl From<crate::harness::Error> for Error {
    fn from(error: crate::harness::Error) -> Self {
        Self::Config(error.to_string())
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Config(_) | Self::Protocol(_) | Self::Decode(_) | Self::Json(_) => {
                StatusCode::BAD_REQUEST
            }
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Provider(_) | Self::Http(_) => StatusCode::BAD_GATEWAY,
            Self::Cancelled => StatusCode::CONFLICT,
            Self::Encode(_) | Self::Io(_) | Self::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
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
