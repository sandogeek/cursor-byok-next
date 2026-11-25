//! Publishes configured Magpie models into Cursor's picker.

use axum::{
    body::Body,
    extract::Extension,
    http::{Request, Response},
};
use prost::Message;

use crate::{
    Result,
    backend::proxy::{self, CursorProxy},
    plugins::{PluginHost, PluginModelDescriptor},
    protocol::{self, proto::agent::v1 as agent},
};

const CLI_LOCAL_MODEL_API_KEY: &str = "cursor-byok-next-local";

const CONTEXTS: [(&str, &str); 5] = [
    ("200k", "200K"),
    ("356k", "356K"),
    ("500k", "500K"),
    ("800k", "800K"),
    ("1m", "1M"),
];
const EFFORTS: [(&str, &str); 5] = [
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "Extra High"),
    ("max", "Max"),
];
const DEFAULT_CONTEXT: &str = "200k";
/// Cursor shows no degradation warning for `0`.
const DEGRADATION_NONE: i32 = 0;
/// Section index of the plugin group in the model picker.
const NAMED_MODEL_SECTION: i32 = 1;
/// Picker badges use `1` for the plain text variant.
const BADGE_TEXT_VARIANT: i32 = 1;
/// Vendor id Cursor reserves for user-provided models.
const VENDOR_ID_BYOK: i32 = 6;

#[derive(Clone, PartialEq, Message)]
struct AvailableModelsAddition {
    #[prost(string, repeated, tag = "1")]
    model_names: Vec<String>,
    #[prost(message, repeated, tag = "2")]
    models: Vec<AvailableModel>,
}

#[derive(Clone, PartialEq, Message)]
struct AvailableModel {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(bool, tag = "2")]
    default_on: bool,
    #[prost(bool, optional, tag = "5")]
    supports_agent: Option<bool>,
    #[prost(int32, optional, tag = "6")]
    degradation_status: Option<i32>,
    #[prost(message, optional, tag = "8")]
    tooltip_data: Option<TooltipData>,
    #[prost(bool, optional, tag = "9")]
    supports_thinking: Option<bool>,
    #[prost(bool, optional, tag = "10")]
    supports_images: Option<bool>,
    #[prost(bool, optional, tag = "14")]
    supports_max_mode: Option<bool>,
    #[prost(string, optional, tag = "17")]
    client_display_name: Option<String>,
    #[prost(string, optional, tag = "18")]
    server_model_name: Option<String>,
    #[prost(bool, optional, tag = "19")]
    supports_non_max_mode: Option<bool>,
    #[prost(message, optional, tag = "20")]
    tooltip_data_for_max_mode: Option<TooltipData>,
    #[prost(bool, optional, tag = "21")]
    is_recommended_for_background_composer: Option<bool>,
    #[prost(bool, optional, tag = "22")]
    supports_plan_mode: Option<bool>,
    #[prost(string, optional, tag = "24")]
    inputbox_short_model_name: Option<String>,
    #[prost(bool, optional, tag = "25")]
    supports_sandboxing: Option<bool>,
    #[prost(bool, optional, tag = "26")]
    supports_cmd_k: Option<bool>,
    #[prost(message, repeated, tag = "29")]
    parameter_definitions: Vec<ModelParameterDefinition>,
    #[prost(message, repeated, tag = "30")]
    variants: Vec<ModelVariant>,
    #[prost(string, repeated, tag = "36")]
    legacy_slugs: Vec<String>,
    #[prost(int32, optional, tag = "38")]
    named_model_section_index: Option<i32>,
    #[prost(string, optional, tag = "41")]
    vendor_name: Option<String>,
    #[prost(message, optional, tag = "42")]
    vendor: Option<AvailableModelVendor>,
    #[prost(message, repeated, tag = "48")]
    model_picker_badges: Vec<ModelPickerBadge>,
}

#[derive(Clone, PartialEq, Message)]
struct TooltipData {
    #[prost(string, optional, tag = "7")]
    markdown_content: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ModelParameterDefinition {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    name: String,
    #[prost(string, optional, tag = "3")]
    markdown_tooltip: Option<String>,
    #[prost(message, optional, tag = "4")]
    parameter_type: Option<ModelParameterType>,
    #[prost(bool, optional, tag = "5")]
    is_cycleable_by_hotkey: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
struct ModelParameterType {
    #[prost(message, optional, tag = "1")]
    boolean_parameter: Option<BooleanParameter>,
    #[prost(message, optional, tag = "2")]
    enum_parameter: Option<EnumParameter>,
}

#[derive(Clone, PartialEq, Message)]
struct BooleanParameter {
    #[prost(message, repeated, tag = "1")]
    values: Vec<BooleanParameterValue>,
}

#[derive(Clone, PartialEq, Message)]
struct BooleanParameterValue {
    #[prost(string, tag = "1")]
    value: String,
    #[prost(string, optional, tag = "2")]
    display_name: Option<String>,
    #[prost(bool, optional, tag = "3")]
    increases_model_cost: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
struct EnumParameter {
    #[prost(message, repeated, tag = "1")]
    values: Vec<EnumParameterValue>,
}

#[derive(Clone, PartialEq, Message)]
struct EnumParameterValue {
    #[prost(string, tag = "1")]
    value: String,
    #[prost(string, optional, tag = "2")]
    display_name: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ModelVariant {
    #[prost(message, repeated, tag = "1")]
    parameter_values: Vec<ModelParameterValue>,
    #[prost(string, tag = "2")]
    display_name: String,
    #[prost(bool, tag = "3")]
    is_max_mode: bool,
    #[prost(bool, optional, tag = "4")]
    is_default_max_config: Option<bool>,
    #[prost(bool, optional, tag = "5")]
    is_default_non_max_config: Option<bool>,
    #[prost(message, optional, tag = "6")]
    tooltip_data: Option<TooltipData>,
    #[prost(string, optional, tag = "8")]
    display_name_outside_picker: Option<String>,
    #[prost(string, optional, tag = "9")]
    variant_string_representation: Option<String>,
    #[prost(string, optional, tag = "11")]
    legacy_slug: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ModelParameterValue {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    value: String,
}

#[derive(Clone, PartialEq, Message)]
struct ModelPickerBadge {
    #[prost(string, tag = "1")]
    label: String,
    #[prost(int32, tag = "2")]
    variant: i32,
    #[prost(bool, tag = "3")]
    dismiss_on_selection: bool,
}

#[derive(Clone, PartialEq, Message)]
struct AvailableModelVendor {
    #[prost(int32, tag = "1")]
    id: i32,
    #[prost(string, tag = "2")]
    display_name: String,
}

#[derive(Clone, PartialEq, Message)]
struct UsableModelsAddition {
    #[prost(message, repeated, tag = "1")]
    models: Vec<agent::ModelDetails>,
}

#[derive(Clone, PartialEq, Message)]
struct DefaultModelResponse {
    #[prost(string, tag = "1")]
    model: String,
    #[prost(string, tag = "2")]
    thinking_model: String,
    #[prost(bool, tag = "3")]
    max_mode: bool,
    #[prost(string, tag = "4")]
    next_default_set_date: String,
}

#[derive(Clone, PartialEq, Message)]
struct DefaultModelNudgeDataResponse {
    #[prost(string, tag = "1")]
    nudge_date: String,
    #[prost(bool, tag = "2")]
    should_default_switch_on_new_chat: bool,
    #[prost(string, repeated, tag = "3")]
    models_with_no_default_switch: Vec<String>,
    #[prost(string, tag = "4")]
    conversion_model_override: String,
}

/// The context-length presets the picker offers.
///
/// Plugin models declare no context window, so the presets are the fixed set
/// Cursor ships with.
fn context_options() -> &'static [(&'static str, &'static str)] {
    &CONTEXTS
}

/// Merges the configured plugin models into Cursor's `AvailableModels`, falling back to the local catalog when upstream fails.
pub async fn available_models(
    Extension(plugins): Extension<PluginHost>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let plugin_models = plugins.configured_models().await;
    tracing::info!(
        plugin_model_count = plugin_models.len(),
        "appending Magpie models to Cursor AvailableModels"
    );
    let local = AvailableModelsAddition {
        model_names: plugin_models.iter().map(|model| model.id.clone()).collect(),
        models: plugin_models.iter().map(available_plugin_model).collect(),
    }
    .encode_to_vec();
    match proxy::forward_buffered(&proxy, request).await {
        Ok(upstream) => merge_response(upstream, local),
        Err(error) => {
            tracing::warn!(%error, "Cursor AvailableModels upstream unavailable; using local catalog");
            Ok(super::proto_bytes_response(local))
        }
    }
}

/// Merges the configured plugin models into `GetUsableModels`, which is what the CLI reads.
pub async fn usable_models(
    Extension(plugins): Extension<PluginHost>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let plugin_models = plugins.configured_models().await;
    tracing::info!(
        plugin_model_count = plugin_models.len(),
        "appending Magpie models to Cursor GetUsableModels"
    );
    let local = UsableModelsAddition {
        models: plugin_models.iter().map(usable_plugin_model).collect(),
    }
    .encode_to_vec();
    match proxy::forward_buffered(&proxy, request).await {
        Ok(upstream) => merge_response(upstream, local),
        Err(error) => {
            tracing::warn!(%error, "Cursor GetUsableModels upstream unavailable; using local catalog");
            Ok(super::proto_bytes_response(local))
        }
    }
}

/// Answers `GetDefaultModelForCli` with the first configured plugin model.
pub async fn default_model_for_cli(
    Extension(plugins): Extension<PluginHost>,
) -> Result<Response<Body>> {
    let plugin_models = plugins.configured_models().await;
    Ok(super::proto_bytes_response(
        agent::GetDefaultModelForCliResponse {
            model: plugin_models.first().map(usable_plugin_model),
        }
        .encode_to_vec(),
    ))
}

/// Answers `GetDefaultModel` with the first configured plugin model, as its own thinking model.
pub async fn default_model(Extension(plugins): Extension<PluginHost>) -> Result<Response<Body>> {
    let plugin_models = plugins.configured_models().await;
    let model = plugin_models
        .first()
        .map(|model| model.id.clone())
        .unwrap_or_default();
    Ok(super::proto_bytes_response(
        DefaultModelResponse {
            thinking_model: model.clone(),
            model,
            max_mode: false,
            next_default_set_date: String::new(),
        }
        .encode_to_vec(),
    ))
}

/// Answers `GetDefaultModelNudgeData`: no nudge, and every plugin model exempt from a default switch.
pub async fn default_model_nudge(
    Extension(plugins): Extension<PluginHost>,
) -> Result<Response<Body>> {
    let plugin_models = plugins.configured_models().await;
    Ok(super::proto_bytes_response(
        DefaultModelNudgeDataResponse {
            nudge_date: "0".into(),
            should_default_switch_on_new_chat: false,
            models_with_no_default_switch: plugin_models
                .iter()
                .map(|model| model.id.clone())
                .collect(),
            conversion_model_override: String::new(),
        }
        .encode_to_vec(),
    ))
}

fn merge_response(upstream: proxy::BufferedResponse, extra: Vec<u8>) -> Result<Response<Body>> {
    if !upstream.status.is_success() {
        tracing::warn!(status = %upstream.status, "Cursor model catalog upstream rejected request; using local catalog");
        return Ok(super::proto_bytes_response(extra));
    }
    let body = protocol::rewrite_unary(&upstream.body, "model catalog", |payload| {
        let mut merged = Vec::with_capacity(payload.len() + extra.len());
        merged.extend_from_slice(payload);
        merged.extend_from_slice(&extra);
        Ok(merged)
    })?;
    Ok(upstream.with_body(body))
}

/// Whether a variant turns on the "Fast" cost tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FastMode {
    Off,
    On,
}

impl FastMode {
    const ALL: [Self; 2] = [Self::Off, Self::On];

    const fn is_on(self) -> bool {
        matches!(self, Self::On)
    }
}

fn available_plugin_model(model: &PluginModelDescriptor) -> AvailableModel {
    let tooltip = TooltipData {
        markdown_content: model.description.clone(),
    };
    let contexts = context_options();
    let variants = model_variants(&model.id, &model.display_name, &tooltip, contexts);
    let legacy_slugs = variants
        .iter()
        .filter_map(|variant| variant.legacy_slug.clone())
        .collect();
    AvailableModel {
        name: model.id.clone(),
        default_on: true,
        supports_agent: Some(true),
        degradation_status: Some(DEGRADATION_NONE),
        tooltip_data: Some(tooltip.clone()),
        supports_thinking: Some(true),
        supports_images: Some(model.images),
        supports_max_mode: Some(false),
        client_display_name: Some(model.display_name.clone()),
        server_model_name: Some(model.id.clone()),
        supports_non_max_mode: Some(true),
        tooltip_data_for_max_mode: Some(tooltip),
        is_recommended_for_background_composer: Some(false),
        supports_plan_mode: Some(true),
        inputbox_short_model_name: Some(model.display_name.clone()),
        supports_sandboxing: Some(true),
        supports_cmd_k: Some(false),
        parameter_definitions: model_parameters(contexts),
        variants,
        legacy_slugs,
        named_model_section_index: Some(NAMED_MODEL_SECTION),
        vendor_name: Some(model.provider_type.clone()),
        vendor: Some(AvailableModelVendor {
            id: VENDOR_ID_BYOK,
            display_name: model.provider_type.clone(),
        }),
        model_picker_badges: vec![ModelPickerBadge {
            label: model.plugin_name.clone(),
            variant: BADGE_TEXT_VARIANT,
            dismiss_on_selection: false,
        }],
    }
}

fn model_parameters(contexts: &[(&str, &str)]) -> Vec<ModelParameterDefinition> {
    let mut parameters = vec![ModelParameterDefinition {
        id: "context".into(),
        name: "Context".into(),
        markdown_tooltip: Some("Context size used to trigger conversation compaction.".into()),
        parameter_type: Some(ModelParameterType {
            boolean_parameter: None,
            enum_parameter: Some(EnumParameter {
                values: contexts
                    .iter()
                    .map(|(value, display_name)| EnumParameterValue {
                        value: (*value).into(),
                        display_name: Some((*display_name).into()),
                    })
                    .collect(),
            }),
        }),
        is_cycleable_by_hotkey: Some(false),
    }];
    // Cursor only shows the effort picker when the model declares the parameter,
    // so every plugin model advertises it even when the provider ignores it.
    parameters.push(ModelParameterDefinition {
        id: "reasoning".into(),
        name: "Effort".into(),
        markdown_tooltip: Some("Effort the model uses to generate its response.".into()),
        parameter_type: Some(ModelParameterType {
            boolean_parameter: None,
            enum_parameter: Some(EnumParameter {
                values: EFFORTS
                    .into_iter()
                    .map(|(value, display_name)| EnumParameterValue {
                        value: value.into(),
                        display_name: Some(display_name.into()),
                    })
                    .collect(),
            }),
        }),
        is_cycleable_by_hotkey: Some(true),
    });
    parameters.push(ModelParameterDefinition {
        id: "fast".into(),
        name: "Fast".into(),
        markdown_tooltip: Some("Significantly faster but consumes more usage".into()),
        parameter_type: Some(ModelParameterType {
            boolean_parameter: Some(BooleanParameter {
                values: vec![
                    BooleanParameterValue {
                        value: "false".into(),
                        display_name: None,
                        increases_model_cost: None,
                    },
                    BooleanParameterValue {
                        value: "true".into(),
                        display_name: Some("Fast".into()),
                        increases_model_cost: Some(true),
                    },
                ],
            }),
            enum_parameter: None,
        }),
        is_cycleable_by_hotkey: Some(false),
    });
    parameters
}

/// The naming context shared by every variant of one model.
struct VariantLabels<'a> {
    name: &'a str,
    display_name: &'a str,
    context: &'a str,
    context_name: &'a str,
}

/// The two axes a variant selects on: reasoning effort and the Fast tier.
#[derive(Clone, Copy)]
struct VariantSelection<'a> {
    effort: (&'a str, &'a str),
    fast: FastMode,
}

fn model_variants(
    name: &str,
    display_name: &str,
    tooltip: &TooltipData,
    contexts: &[(&str, &str)],
) -> Vec<ModelVariant> {
    let mut variants = Vec::with_capacity(contexts.len() * EFFORTS.len() * FastMode::ALL.len());
    for (context, context_name) in contexts {
        let labels = VariantLabels {
            name,
            display_name,
            context,
            context_name,
        };
        for effort in EFFORTS {
            for fast in FastMode::ALL {
                variants.push(model_variant(
                    &labels,
                    tooltip,
                    VariantSelection { effort, fast },
                ));
            }
        }
    }
    variants
}

fn model_variant(
    labels: &VariantLabels<'_>,
    tooltip: &TooltipData,
    selection: VariantSelection<'_>,
) -> ModelVariant {
    let VariantLabels {
        name,
        display_name,
        context,
        context_name,
    } = *labels;
    let VariantSelection { effort, fast } = selection;
    let (effort, effort_name) = effort;
    let fast = fast.is_on();
    let mut suffix = Vec::with_capacity(3);
    if context != DEFAULT_CONTEXT {
        suffix.push(context_name);
    }
    suffix.push(effort_name);
    if fast {
        suffix.push("Fast");
    }
    let suffix = suffix.join(" ");
    let display_name = if suffix.is_empty() {
        display_name.to_owned()
    } else {
        format!(
            "{display_name} <span style=\"color: var(--cursor-text-tertiary);\">{suffix}</span>"
        )
    };
    let is_default = context == DEFAULT_CONTEXT && !fast && effort == "high";
    let parameter_values = vec![
        ModelParameterValue {
            id: "context".into(),
            value: context.into(),
        },
        ModelParameterValue {
            id: "reasoning".into(),
            value: effort.into(),
        },
        ModelParameterValue {
            id: "fast".into(),
            value: fast.to_string(),
        },
    ];
    ModelVariant {
        parameter_values,
        display_name: display_name.clone(),
        is_max_mode: false,
        is_default_max_config: is_default.then_some(true),
        is_default_non_max_config: is_default.then_some(true),
        tooltip_data: Some(tooltip.clone()),
        display_name_outside_picker: Some(display_name),
        variant_string_representation: Some(format!(
            "{name}[context={context},reasoning={effort},fast={fast}]"
        )),
        legacy_slug: Some(format!(
            "{name}-{context}-{effort}{}",
            if fast { "-fast" } else { "" }
        )),
    }
}

fn cli_local_model_credentials() -> agent::model_details::Credentials {
    agent::model_details::Credentials::ApiKeyCredentials(agent::ApiKeyCredentials {
        api_key: CLI_LOCAL_MODEL_API_KEY.into(),
        base_url: None,
    })
}

fn usable_plugin_model(model: &PluginModelDescriptor) -> agent::ModelDetails {
    agent::ModelDetails {
        model_id: model.id.clone(),
        display_model_id: model.id.clone(),
        display_name: model.display_name.clone(),
        display_name_short: model.display_name.clone(),
        thinking_details: Some(agent::ThinkingDetails::default()),
        credentials: Some(cli_local_model_credentials()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_plugin_model_details_use_local_routing_credentials() {
        let details = usable_plugin_model(&PluginModelDescriptor {
            id: "plugin:test/provider/model".into(),
            plugin_id: "plugin:test".into(),
            plugin_name: "Test Plugin".into(),
            provider_id: "provider".into(),
            model_id: "model".into(),
            display_name: "Plugin Model".into(),
            description: None,
            provider_type: "test".into(),
            max_output_tokens: None,
            images: false,
        });
        assert_eq!(details.model_id, "plugin:test/provider/model");
        let agent::model_details::Credentials::ApiKeyCredentials(credentials) =
            details.credentials.expect("API credentials")
        else {
            panic!("expected API key credentials");
        };
        assert_eq!(credentials.api_key, CLI_LOCAL_MODEL_API_KEY);
        assert_eq!(credentials.base_url, None);
    }
}
