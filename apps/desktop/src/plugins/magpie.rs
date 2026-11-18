//! Magpie catalog parsing invariants, independent of the Bun worker.

use super::{state::StoredModel, Error, Result};

fn object(value: &serde_json::Value) -> Option<&serde_json::Map<String, serde_json::Value>> {
    value.as_object()
}

fn text(value: Option<&serde_json::Value>) -> Option<String> {
    value
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn positive_integer(value: Option<&serde_json::Value>) -> Option<u64> {
    match value {
        Some(serde_json::Value::Number(number)) => number.as_u64().filter(|value| *value > 0),
        Some(serde_json::Value::String(text)) => text.parse::<u64>().ok().filter(|value| *value > 0),
        _ => None,
    }
}

fn native_endpoints(value: Option<&serde_json::Value>) -> Vec<String> {
    let Some(serde_json::Value::Array(items)) = value else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    let mut endpoints = Vec::new();
    for item in items {
        if let Some(text) = item.as_str().map(str::trim).filter(|value| !value.is_empty())
            && seen.insert(text.to_owned())
        {
            endpoints.push(text.to_owned());
        }
    }
    endpoints
}

fn is_media_kind(value: Option<&serde_json::Value>) -> bool {
    matches!(
        value.and_then(serde_json::Value::as_str).map(str::to_ascii_lowercase).as_deref(),
        Some("image" | "video")
    )
}

pub fn parse_magpie_models(body: &serde_json::Value) -> Result<Vec<StoredModel>> {
    let source = object(body)
        .and_then(|root| root.get("data").or_else(|| root.get("models")))
        .unwrap_or(body);
    let Some(items) = source.as_array() else {
        return Err(Error::Protocol(
            "Magpie model discovery response does not contain a model list".into(),
        ));
    };
    let mut seen = std::collections::HashSet::new();
    let mut models = Vec::new();
    for raw in items {
        let Some(model) = object(raw) else {
            continue;
        };
        if is_media_kind(model.get("kind")) {
            continue;
        }
        let Some(id) = text(model.get("id")) else {
            continue;
        };
        if !seen.insert(id.clone()) {
            continue;
        }
        let display_name = text(model.get("magpie_label"))
            .or_else(|| text(model.get("display_name")))
            .or_else(|| text(model.get("displayName")))
            .unwrap_or_else(|| id.clone());
        models.push(StoredModel {
            id,
            display_name,
            description: None,
            max_output_tokens: positive_integer(
                model
                    .get("max_output_tokens")
                    .or_else(|| model.get("maxOutputTokens")),
            ),
            images: false,
            private_data: serde_json::json!({
                "nativeEndpoints": native_endpoints(
                    model.get("native_endpoints").or_else(|| model.get("nativeEndpoints"))
                ),
            }),
        });
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_keeps_chat_models_drops_media_and_preserves_native_endpoints() {
        let models = parse_magpie_models(&serde_json::json!({
            "data": [
                {
                    "id": "deepseek/deepseek-v4-pro",
                    "display_name": "DeepSeek V4 Pro",
                    "magpie_label": "DeepSeek V4 Pro · local",
                    "native_endpoints": ["/v1/chat/completions"],
                    "max_output_tokens": 8192
                },
                {
                    "id": "openai/gpt-5",
                    "display_name": "GPT-5",
                    "native_endpoints": ["/v1/responses", "/v1/chat/completions"]
                },
                { "id": "black-forest/flux", "kind": "image" },
                { "id": "openai/sora", "kind": "video" },
                { "id": "deepseek/deepseek-v4-pro", "display_name": "duplicate" }
            ]
        }))
        .unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].display_name, "DeepSeek V4 Pro · local");
        assert_eq!(models[0].max_output_tokens, Some(8192));
        assert_eq!(
            models[0].private_data["nativeEndpoints"][0],
            "/v1/chat/completions"
        );
        assert_eq!(models[1].id, "openai/gpt-5");
        assert!(parse_magpie_models(&serde_json::json!({ "data": [] }))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn stored_model_from_definition_keeps_private_data_out_of_display_fields() {
        let model = StoredModel::from_definition(&serde_json::json!({
            "id": "openai/gpt-5",
            "displayName": "GPT-5",
            "privateData": { "nativeEndpoints": ["/v1/responses"] }
        }))
        .unwrap();
        assert_eq!(model.display_name, "GPT-5");
        assert_eq!(model.private_data["nativeEndpoints"][0], "/v1/responses");
    }
}
