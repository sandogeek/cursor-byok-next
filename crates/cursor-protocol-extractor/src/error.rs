//! Errors raised while extracting or parsing protobuf declarations.

use std::fmt;

use oxc::diagnostics::OxcDiagnostic;

/// A failure while scanning a bundle or parsing a single field declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum Error {
    /// A JS source could not be parsed. The first oxc diagnostic is kept as the
    /// [`source`](std::error::Error::source); `other_errors` counts the rest so
    /// the message does not hide them.
    Parse {
        /// The first diagnostic oxc reported.
        diagnostic: OxcDiagnostic,
        /// How many further diagnostics oxc reported.
        other_errors: usize,
    },
    /// A field object or compact field string was missing required data.
    Field(String),
    /// A generated proto file failed the local-loop shape checks.
    Shape(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse {
                diagnostic,
                other_errors,
            } => {
                write!(f, "cannot parse JS source: {diagnostic}")?;
                match other_errors {
                    0 => Ok(()),
                    1 => f.write_str(" (1 more diagnostic)"),
                    count => write!(f, " ({count} more diagnostics)"),
                }
            }
            Self::Field(message) | Self::Shape(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse { diagnostic, .. } => Some(diagnostic),
            Self::Field(_) | Self::Shape(_) => None,
        }
    }
}
