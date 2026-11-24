//! Adapts Cursor's client-owned Agent loop to configured plugin providers.

mod inference;
mod output;
mod overflow;
mod project;
mod subagent_models;

pub(crate) use inference::{InferenceRoute, InferenceService, decode_append};
