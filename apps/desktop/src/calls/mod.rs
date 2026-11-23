//! Records Magpie provider calls for the Calls screen.

mod recorder;
mod store;

pub use recorder::CallRecorder;
pub use store::{
    CallSnapshot, CallStorage, CallStore, LlmCallRequest, LlmCallResponseChunk, LlmCallSummary,
    NewLlmCall,
};

pub fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "x-api-key" | "api-key" | "cookie" | "set-cookie"
    )
}

pub fn plugin_request_url(model_id: &str) -> (String, String, String) {
    crate::plugins::parse_model_id(model_id).map_or_else(
        || {
            (
                model_id.to_owned(),
                "plugin".into(),
                format!("plugin://{model_id}"),
            )
        },
        |(plugin_id, provider_id, stored_id)| {
            (
                stored_id.to_owned(),
                "plugin".into(),
                format!("plugin://{plugin_id}/{provider_id}"),
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::plugin_request_url;

    #[test]
    fn plugin_request_url_uses_plugin_and_provider() {
        let (model, kind, url) = plugin_request_url(
            "plugin:dev.cursorbyoknext.plugins.magpie/magpie/resource-1/openai/gpt-5",
        );
        assert_eq!(model, "resource-1/openai/gpt-5");
        assert_eq!(kind, "plugin");
        assert_eq!(url, "plugin://dev.cursorbyoknext.plugins.magpie/magpie");
    }
}
