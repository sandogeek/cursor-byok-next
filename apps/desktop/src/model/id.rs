//! Stable `plugin:<plugin>/<provider>/<model>` identifier parsing and formatting.

pub const ADAPTER_ID_PREFIX: &str = "plugin:";

/// The `plugin:<plugin>/<provider>/<model>` id for a stored model.
///
/// Not to be confused with [`parse_model_id`], which splits one back apart.
pub fn model_id(plugin_id: &str, provider_id: &str, upstream_model: &str) -> String {
    format!("{ADAPTER_ID_PREFIX}{plugin_id}/{provider_id}/{upstream_model}")
}

/// A `plugin:<plugin>/<provider>/<model>` id split into its three parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelId<'a> {
    /// The plugin that owns the provider.
    pub plugin: &'a str,
    /// The provider inside that plugin.
    pub provider: &'a str,
    /// The provider's own model id; may itself contain slashes.
    pub model: &'a str,
}

/// Splits a plugin model id, or returns `None` when `value` is not one.
pub fn parse_model_id(value: &str) -> Option<ModelId<'_>> {
    let rest = value.strip_prefix(ADAPTER_ID_PREFIX)?;
    let (plugin, rest) = rest.split_once('/')?;
    let (provider, model) = rest.split_once('/')?;
    let id = ModelId {
        plugin,
        provider,
        model,
    };
    (!plugin.is_empty() && !provider.is_empty() && !model.is_empty()).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stable_model_ids_with_slashes() {
        let id = model_id("dev.example", "codex", "org/gpt-5");
        assert_eq!(
            parse_model_id(&id),
            Some(ModelId {
                plugin: "dev.example",
                provider: "codex",
                model: "org/gpt-5",
            })
        );
        assert_eq!(
            parse_model_id("plugin:dev.example/gateway/resource-id/gpt-5"),
            Some(ModelId {
                plugin: "dev.example",
                provider: "gateway",
                model: "resource-id/gpt-5",
            })
        );
        assert_eq!(parse_model_id("model-hash"), None);
    }
}
