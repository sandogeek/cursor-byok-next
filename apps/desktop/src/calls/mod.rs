//! Records Magpie provider calls for the Calls screen.

mod recorder;
mod store;

pub use recorder::CallRecorder;
pub use store::{
    CallOutcome, CallSnapshot, CallStatus, CallStorage, CallStore, DEFAULT_LIST_LIMIT,
    LlmCallRequest, LlmCallResponseChunk, LlmCallSummary, NewLlmCall, PayloadStorage,
};

/// Whether a header must be kept out of the call log. These are the header
/// names that carry credentials across the providers we serve, so the log
/// filters them by name rather than trying to recognize key shapes.
pub fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "x-api-key" | "api-key" | "cookie" | "set-cookie"
    )
}

/// Where a plugin model's calls go, for the call records screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRequestUrl {
    /// The provider's own model id, without the plugin prefix.
    pub model: String,
    /// The provider family recorded for the call.
    pub provider: String,
    /// The `plugin://<plugin>/<provider>` URL the provider call went to.
    pub url: String,
}

/// Builds the call-record coordinates for a Cursor model id.
///
/// A model id that is not a plugin id is reported as its own model under a
/// `plugin://` URL built from the id itself.
pub fn plugin_request_url(model_id: &str) -> PluginRequestUrl {
    crate::plugins::parse_model_id(model_id).map_or_else(
        || PluginRequestUrl {
            model: model_id.to_owned(),
            provider: "plugin".into(),
            url: format!("plugin://{model_id}"),
        },
        |id| PluginRequestUrl {
            model: id.model.to_owned(),
            provider: "plugin".into(),
            url: format!("plugin://{}/{}", id.plugin, id.provider),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::plugin_request_url;

    #[test]
    fn plugin_request_url_uses_plugin_and_provider() {
        let request = plugin_request_url(
            "plugin:dev.cursorbyoknext.plugins.magpie/magpie/resource-1/openai/gpt-5",
        );
        assert_eq!(request.model, "resource-1/openai/gpt-5");
        assert_eq!(request.provider, "plugin");
        assert_eq!(
            request.url,
            "plugin://dev.cursorbyoknext.plugins.magpie/magpie"
        );
    }
}
