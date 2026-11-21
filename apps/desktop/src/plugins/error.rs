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

pub type Result<T, E = Error> = std::result::Result<T, E>;
