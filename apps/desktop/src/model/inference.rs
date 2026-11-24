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
    /// The `plugin:`/provider hash the caller routed to.
    pub model_id: String,
    /// The name to show in recorded calls; the provider's own id when absent.
    pub display_name: Option<String>,
    /// Whether reasoning is on, and at which depth.
    pub reasoning: Reasoning,
    /// Whether the caller asked for the provider's fast tier.
    pub latency: ModelLatency,
    /// When absent, the provider's default applies.
    pub max_output_tokens: Option<u64>,
    /// The window the client asked for, which bounds compaction, not the
    /// provider's own limit.
    pub context_window_tokens: Option<u64>,
    /// Sequences that end generation before the model would.
    #[serde(default)]
    pub stop_sequences: Vec<String>,
    /// Provider-specific sampling fields, merged into the request body by each
    /// adapter; a field a protocol owns (`model`, `messages`, `tools`, …) is
    /// rejected rather than silently replaced.
    #[serde(default)]
    pub extra_params: serde_json::Value,
}

impl ModelSpec {
    /// A spec for `model_id` with every optional knob left at its default.
    #[must_use]
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
    /// The system prompt: rules, agent instructions and the model catalog.
    pub instructions: String,
    /// Tools the provider may call for this turn.
    pub tools: Vec<ToolDefinition>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelRequest {
    /// The system prompt and tools of this turn.
    pub prompt: PromptSpec,
    /// Which model to call, and with which sampling knobs.
    pub model: ModelSpec,
    /// The conversation as the client sent it, oldest first.
    pub history: Vec<ProjectedMessage>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelInvocation {
    /// The call id the recorder and the invocation responses share.
    pub call_id: String,
    /// The client Run the call belongs to.
    pub run_id: String,
    /// Stable per conversation, so providers and plugins can key their caches.
    pub conversation_id: String,
    /// 0-based index of this call within the run, for the call log.
    pub provider_call_index: u64,
    /// Everything the provider is asked for.
    pub request: ModelRequest,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    /// Prompt tokens as the provider bills them.
    pub input_tokens: Option<u64>,
    /// Prompt tokens that actually occupied the context window, when a provider
    /// reports it separately (cache reads do not).
    pub context_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    /// Prompt tokens served from the provider's cache.
    pub cache_read_tokens: Option<u64>,
    /// Prompt tokens written to the provider's cache.
    pub cache_write_tokens: Option<u64>,
    /// Output tokens the model spent on hidden reasoning.
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
