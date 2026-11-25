//! Provider-independent model types used by the local loop.

mod id;
mod inference;
mod message;
mod projection;
mod token_count;
mod tool;

pub use id::{ADAPTER_ID_PREFIX, ModelId, model_id, parse_model_id};

pub use inference::{
    ModelInvocation, ModelLatency, ModelRequest, ModelSpec, PromptSpec, Reasoning, Usage,
};
pub use message::{
    ContentPart, ProviderReplayState, Role, ToolCallContent, ToolResultContent, joined_text,
};
pub use projection::{ProjectedContent, ProjectedMessage};
pub use tool::ToolDefinition;

pub(crate) use token_count::parse_token_count;
