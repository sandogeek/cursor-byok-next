//! Errors from discovering plugins, talking to workers, and storing resources.

use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Protocol(String),
    #[error("{0}")]
    Provider(String),
    #[error("{0}")]
    Runtime(String),
    #[error("{0}")]
    NotFound(String),
    #[error("HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("run was cancelled")]
    Cancelled,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Storage(String),
}

impl From<crate::harness::Error> for Error {
    fn from(error: crate::harness::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

impl From<crate::Error> for Error {
    fn from(error: crate::Error) -> Self {
        match error {
            crate::Error::Config(message) => Self::Config(message),
            crate::Error::Protocol(message) => Self::Protocol(message),
            crate::Error::Provider(message) => Self::Provider(message),
            crate::Error::NotFound(message) => Self::NotFound(message),
            crate::Error::Cancelled => Self::Cancelled,
            crate::Error::Json(error) => Self::Json(error),
            crate::Error::Io(error) => Self::Io(error),
            crate::Error::Storage(_)
            | crate::Error::Http(_)
            | crate::Error::Decode(_)
            | crate::Error::Encode(_) => Self::Storage(error.to_string()),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
