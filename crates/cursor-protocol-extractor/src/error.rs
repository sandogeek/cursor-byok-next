//! Errors raised while extracting or parsing protobuf declarations.

use std::fmt;

/// A failure while scanning a bundle or parsing a single field declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A JS expression did not have the shape the scanner expected.
    Parse(String),
    /// A field object or compact field string was missing required data.
    Field(String),
    /// A generated proto file failed the local-loop shape checks.
    Shape(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(message) | Self::Field(message) | Self::Shape(message) => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for Error {}
