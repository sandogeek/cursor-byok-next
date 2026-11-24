//! Normalized provider streaming events.

use std::pin::Pin;

use futures_util::Stream;

use crate::{
    Result,
    model::{ProviderReplayState, Usage},
};

pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ModelEvent>> + Send>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishReason {
    /// The model finished its turn on its own.
    Stop,
    /// The model hit a token or context limit.
    Length,
    /// The model asked to call a tool.
    ToolUse,
}

/// One normalized event from a provider stream.
///
/// The loop's event grammar is: `Start`, then any interleaving of text,
/// thinking, and tool-call blocks (each opened by a `*Start` and closed by its
/// `*End`), then at most one `Usage`, then `Done`.
#[derive(Clone, Debug, PartialEq)]
pub enum ModelEvent {
    /// The call began; `model_call_id` attributes the recorded call.
    Start {
        model_call_id: String,
    },
    /// Opens a text block. Deltas before this are a provider bug.
    TextStart,
    TextDelta(String),
    TextEnd,
    /// Opens a thinking block; a closing one is always emitted.
    ThinkingStart,
    ThinkingDelta(String),
    ThinkingEnd,
    /// A provider-signed thinking summary, replayed back to the provider.
    ThinkingSignature(String),
    /// Opaque reasoning the provider hid but wants back verbatim.
    RedactedThinking(String),
    /// Opens the tool call at `index`; `name` is Cursor's original tool name.
    ToolCallStart {
        index: usize,
        call_id: String,
        name: String,
    },
    ToolCallArgumentsDelta {
        index: usize,
        delta: String,
    },
    ToolCallEnd {
        index: usize,
    },
    /// Provider-native state to hand back on the next turn.
    ProviderReplayState(ProviderReplayState),
    Usage(Usage),
    /// The terminal event; a stream without it is an incomplete turn.
    Done(FinishReason),
}

/// Whether `event` may be relayed to Cursor as model output. Start, usage,
/// replay state, and the terminal marker describe the call itself, so only
/// content events pass.
#[must_use]
pub fn is_valid_response_event(event: &ModelEvent) -> bool {
    matches!(
        event,
        ModelEvent::TextDelta(_)
            | ModelEvent::ThinkingStart
            | ModelEvent::ThinkingDelta(_)
            | ModelEvent::ThinkingEnd
            | ModelEvent::ToolCallStart { .. }
            | ModelEvent::ToolCallArgumentsDelta { .. }
            | ModelEvent::ToolCallEnd { .. }
    )
}
