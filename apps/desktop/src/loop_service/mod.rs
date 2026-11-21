//! Adapts Cursor's client-owned Agent loop to configured plugin providers.

mod output;
mod overflow;
mod project;
mod service;
mod subagent_models;

pub(crate) use service::{InferenceRoute, InferenceService, decode_append};
