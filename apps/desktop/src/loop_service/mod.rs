//! Adapts Cursor's client-owned Agent loop to configured plugin providers.

mod overflow;
mod output;
mod project;
mod service;
mod subagent_models;

pub(crate) use service::{decode_append, InferenceRoute, InferenceService};
