//! Provider-independent model requests.

use serde::{Deserialize, Serialize};

use super::{ProjectedMessage, ToolDefinition};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelLatency {
    #[default]
    Standard,
    Fast,
}

/// How much reasoning a provider should do, when it supports any.
///
/// There is no "effort without enabling" state: a requested effort always
/// turns reasoning on, so the two cannot drift apart.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reasoning {
    /// Reasoning is off; providers omit their thinking blocks entirely.
    #[default]
    Off,
    /// Reasoning is on at the provider's own default depth.
    Adaptive,
    /// Reasoning is on at a named depth (`"low"`, `"medium"`, `"high"`).
    Effort(String),
}

impl Reasoning {
    /// Whether any reasoning was requested.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Off)
    }

    /// The named depth, when the client asked for one.
    #[must_use]
    pub fn effort(&self) -> Option<&str> {
        match self {
            Self::Effort(effort) => Some(effort),
            Self::Off | Self::Adaptive => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelSpec {
    pub model_id: String,
    pub display_name: Option<String>,
    pub reasoning: Reasoning,
    pub latency: ModelLatency,
    pub max_output_tokens: Option<u64>,
    pub context_window_tokens: Option<u64>,
    #[serde(default)]
    pub stop_sequences: Vec<String>,
    #[serde(default)]
    pub extra_params: serde_json::Value,
}

impl ModelSpec {
    pub fn new(model_id: &str) -> Self {
        Self {
            model_id: model_id.to_owned(),
            display_name: None,
            reasoning: Reasoning::default(),
            latency: ModelLatency::Standard,
            max_output_tokens: None,
            context_window_tokens: None,
            stop_sequences: Vec::new(),
            extra_params: serde_json::json!({}),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PromptSpec {
    pub instructions: String,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelRequest {
    pub prompt: PromptSpec,
    pub model: ModelSpec,
    pub history: Vec<ProjectedMessage>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelInvocation {
    pub call_id: String,
    pub run_id: String,
    pub conversation_id: String,
    pub provider_call_index: u64,
    pub request: ModelRequest,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub context_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    /// The three counts every provider reports; the rest default to absent.
    ///
    /// `context_input_tokens` starts as `input_tokens` because most providers
    /// do not distinguish them.
    #[must_use]
    pub fn new(
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        total_tokens: Option<u64>,
    ) -> Self {
        Self {
            input_tokens,
            context_input_tokens: input_tokens,
            output_tokens,
            total_tokens,
            ..Self::default()
        }
    }

    /// The input tokens actually sent, when the provider reports it separately.
    #[must_use]
    pub fn with_context_input(mut self, tokens: Option<u64>) -> Self {
        self.context_input_tokens = tokens;
        self
    }

    /// Tokens served from the provider's prompt cache.
    #[must_use]
    pub fn with_cache_read(mut self, tokens: Option<u64>) -> Self {
        self.cache_read_tokens = tokens;
        self
    }

    /// Tokens written to the provider's prompt cache.
    #[must_use]
    pub fn with_cache_write(mut self, tokens: Option<u64>) -> Self {
        self.cache_write_tokens = tokens;
        self
    }

    /// Tokens the model spent on hidden reasoning.
    #[must_use]
    pub fn with_reasoning(mut self, tokens: Option<u64>) -> Self {
        self.reasoning_tokens = tokens;
        self
    }
}
