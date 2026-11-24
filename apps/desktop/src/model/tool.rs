//! Tool definitions advertised to providers.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One tool Cursor offers the model, in provider-neutral form. Each adapter
/// renames `name` per that protocol's tool-name rules and sends `parameters` as
/// the input schema.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    /// Cursor's own tool name; the adapter aliases it for the provider.
    pub name: String,
    pub description: String,
    /// A JSON Schema object describing the arguments.
    pub parameters: Value,
}
