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
    #[serde(default)]
    pub catalog: CatalogMode,
}

/// How a provider's model list is obtained.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CatalogMode {
    /// Ask the plugin for models the first time they are needed.
    Live,
    /// Only the models the plugin stored when a resource was added.
    #[default]
    Snapshot,
}

impl ProviderDefinition {
    /// The resource type a live catalog is read through, or `None` when this
    /// provider has no live catalog.
    ///
    /// Validation rejects a live catalog without models or without a resource
    /// type, but the compiler cannot see that pair of constraints, so it is
    /// stated once here instead of at every call site.
    #[must_use]
    pub fn live_resource_type(&self) -> Option<&str> {
        if !self.has_models || self.catalog != CatalogMode::Live {
            return None;
        }
        self.resource_type.as_deref()
    }
}

impl CatalogMode {
    /// The wire name Cursor and plugin manifests use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => LIVE_CATALOG,
            Self::Snapshot => SNAPSHOT_CATALOG,
        }
    }
}

impl std::fmt::Display for CatalogMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
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

/// One "add a resource" flow. `fields` belongs to [`AddMethodKind::Form`] and
/// is empty for every other kind; `callback` is the OAuth redirect the host
/// listens on. The two extra kinds the SDK declares (`oauth2.0` and
/// `oauth2.authorization-code`) are rejected at load time, so only `form`
/// reaches this struct today.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AddMethodDefinition {
    #[serde(rename = "type")]
    pub method_type: AddMethodKind,
    pub id: String,
    pub display_name: LocalizedText,
    #[serde(default)]
    pub description: LocalizedText,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback: Option<OAuthCallbackDefinition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FormFieldDefinition>,
}

/// The kinds of "add a resource" flow a plugin can declare.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AddMethodKind {
    /// A form the host draws from [`FormFieldDefinition`]s.
    Form,
}

impl AddMethodKind {
    /// The wire name Cursor and plugin manifests use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Form => FORM_ADD_METHOD,
        }
    }
}

/// The host-rendered input kinds a plugin form may ask for.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FormFieldKind {
    /// A URL the host also registers as a network origin.
    Url,
    /// A value the host masks while typing.
    Secret,
    /// Free text.
    Text,
}

impl FormFieldKind {
    /// The wire name Cursor and plugin manifests use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Url => FORM_FIELD_URL,
            Self::Secret => FORM_FIELD_SECRET,
            Self::Text => FORM_FIELD_TEXT,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormFieldDefinition {
    pub id: String,
    pub label: LocalizedText,
    #[serde(rename = "type")]
    pub field_type: FormFieldKind,
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
    pub catalog: CatalogMode,
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

/// What a plugin's `resource.present` returns for one resource: the display
/// name, an optional localized description, and optional usage metrics.
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
    /// Combines the stored record with the presentation the plugin returned.
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

impl PluginModelDescriptor {
    /// Projects one stored model into the flat shape the model picker reads.
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
