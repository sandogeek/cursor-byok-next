//! Provider-visible projected messages.

use serde::{Deserialize, Serialize};

use super::{ContentPart, ProviderReplayState, Role, ToolCallContent, ToolResultContent};

/// What one projected message carries, mirroring [`Role`]: a system or user
/// turn is parts, an assistant turn is text plus its calls, and a tool turn is
/// one result. The variants carry the role's own data so no role can be read
/// with the wrong shape.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum ProjectedContent {
    /// A system or user turn: text interspersed with images and files.
    Parts(Vec<ContentPart>),
    /// An assistant turn. `text` is only the prose; `thinking` keeps the
    /// summary for display, while `replay_state` is what goes back verbatim.
    Assistant {
        text: String,
        thinking: String,
        replay_state: Option<ProviderReplayState>,
        calls: Vec<ToolCallContent>,
    },
    /// A tool turn: exactly one result, addressed to one call id.
    ToolResult(ToolResultContent),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProjectedMessage {
    /// Cursor's id for the turn, kept stable so appends do not duplicate it.
    pub message_id: String,
    pub role: Role,
    pub content: ProjectedContent,
}
