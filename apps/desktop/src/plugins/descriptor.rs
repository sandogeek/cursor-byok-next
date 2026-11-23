//! Defines serializable plugin capability definitions and desktop descriptors.

use serde::{Deserialize, Serialize};

use super::state::{ResourceRecord, ResourceState, StoredModel};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginModuleDefinition {
    pub providers: Vec<ProviderDefinition>,
    #[serde(default)]
    pub resources: Vec<ResourceDefinition>,
}

pub type LocalizedText = serde_json::Value;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderDefinition {
    pub id: String,
    pub display_name: LocalizedText,
    #[serde(default)]
    pub description: LocalizedText,
    pub provider_type: String,
    #[serde(default)]
    pub resource_type: Option<String>,
    pub has_models: bool,
    #[serde(default = "default_catalog_mode")]
    pub catalog: String,
}

fn default_catalog_mode() -> String {
    SNAPSHOT_CATALOG.to_owned()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceDefinition {
    #[serde(rename = "type")]
    pub resource_type: String,
    pub display_name: LocalizedText,
    #[serde(default)]
    pub add: Vec<AddMethodDefinition>,
    pub can_refresh: bool,
    pub can_remove: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AddMethodDefinition {
    #[serde(rename = "type")]
    pub method_type: String,
    pub id: String,
    pub display_name: LocalizedText,
    #[serde(default)]
    pub description: LocalizedText,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback: Option<OAuthCallbackDefinition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FormFieldDefinition>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormFieldDefinition {
    pub id: String,
    pub label: LocalizedText,
    #[serde(rename = "type")]
    pub field_type: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub placeholder: LocalizedText,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OAuthCallbackDefinition {
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub path: Option<String>,
}

pub const FORM_ADD_METHOD: &str = "form";
pub const SNAPSHOT_CATALOG: &str = "snapshot";
pub const LIVE_CATALOG: &str = "live";
pub const FORM_FIELD_URL: &str = "url";
pub const FORM_FIELD_SECRET: &str = "secret";
pub const FORM_FIELD_TEXT: &str = "text";
pub const ADAPTER_ID_PREFIX: &str = "plugin:";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDescriptor {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: Option<String>,
    pub icon: String,
    pub runtime_ready: bool,
    pub runtime_error: Option<String>,
    pub providers: Vec<PluginProviderDescriptor>,
    pub resources: Vec<PluginResourceDescriptor>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginProviderDescriptor {
    pub id: String,
    pub plugin_id: String,
    pub display_name: LocalizedText,
    pub description: LocalizedText,
    pub provider_type: String,
    pub resource_type: Option<String>,
    pub has_models: bool,
    pub catalog: String,
    pub configured: bool,
    pub models: Vec<PluginModelDescriptor>,
    pub catalog_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginModelDescriptor {
    pub id: String,
    pub plugin_id: String,
    pub plugin_name: String,
    pub provider_id: String,
    pub model_id: String,
    pub display_name: String,
    pub description: Option<String>,
    pub provider_type: String,
    pub max_output_tokens: Option<u64>,
    pub images: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginResourceDescriptor {
    #[serde(rename = "type")]
    pub resource_type: String,
    pub display_name: LocalizedText,
    pub add: Vec<AddMethodDefinition>,
    pub can_refresh: bool,
    pub can_remove: bool,
    pub storage_error: Option<String>,
    pub resources: Vec<PluginResourceView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginResourceView {
    pub id: String,
    pub state: ResourceState,
    pub display_name: String,
    pub description: LocalizedText,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourcePresentation {
    pub display_name: String,
    #[serde(default)]
    pub description: LocalizedText,
    #[serde(default)]
    pub metrics: Vec<serde_json::Value>,
}

impl PluginResourceView {
    pub fn from_record(record: &ResourceRecord, presentation: ResourcePresentation) -> Self {
        Self {
            id: record.id.clone(),
            state: record.state.clone(),
            display_name: presentation.display_name,
            description: presentation.description,
            created_at_ms: record.created_at_ms,
        }
    }
}

pub fn model_id(plugin_id: &str, provider_id: &str, model_id: &str) -> String {
    format!("{ADAPTER_ID_PREFIX}{plugin_id}/{provider_id}/{model_id}")
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

impl PluginModelDescriptor {
    pub fn new(
        plugin_id: &str,
        plugin_name: &str,
        provider: &ProviderDefinition,
        model: &StoredModel,
    ) -> Self {
        Self {
            id: model_id(plugin_id, &provider.id, &model.id),
            plugin_id: plugin_id.to_owned(),
            plugin_name: plugin_name.to_owned(),
            provider_id: provider.id.clone(),
            model_id: model.id.clone(),
            display_name: model.display_name.clone(),
            description: model.description.clone(),
            provider_type: provider.provider_type.clone(),
            max_output_tokens: model.max_output_tokens,
            images: model.images,
        }
    }
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
