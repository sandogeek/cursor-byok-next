//! Configures Cursor feature gates so agent-host chooses the local loop.

use axum::{
    body::{Body, Bytes},
    extract::Extension,
    http::{HeaderValue, Request, Response, StatusCode, header},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use prost::Message;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::{Error, Result, backend::proxy};

pub const BOOTSTRAP_STATSIG_PATH: &str = "/aiserver.v1.AnalyticsService/BootstrapStatsig";
const AGENT_GATES: [(&str, bool); 2] =
    [("nal_agent_retries", true), ("agent_host_local_loop", true)];

/// Whether a Statsig feature gate is on for the local user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Enabled {
    Enabled,
    Disabled,
}

impl From<bool> for Enabled {
    fn from(enabled: bool) -> Self {
        if enabled {
            Self::Enabled
        } else {
            Self::Disabled
        }
    }
}

#[derive(Clone, PartialEq, Message)]
struct BootstrapStatsigResponse {
    #[prost(string, tag = "1")]
    config: String,
    #[prost(uint64, tag = "2")]
    generated_at_ms: u64,
}

pub async fn bootstrap_statsig(
    Extension(upstream): Extension<proxy::CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    match proxy::forward_buffered(&upstream, request).await {
        Ok(response) if response.status.is_success() => match patch_upstream(response) {
            Ok(response) => Ok(response),
            Err(error) => {
                tracing::warn!(%error, "Cursor Statsig bootstrap was invalid; using local bootstrap");
                local_response()
            }
        },
        Ok(response) => {
            tracing::warn!(status = %response.status, "Cursor Statsig bootstrap was rejected; using local bootstrap");
            local_response()
        }
        Err(error) => {
            tracing::warn!(%error, "Cursor Statsig bootstrap was unavailable; using local bootstrap");
            local_response()
        }
    }
}

fn patch_upstream(response: proxy::BufferedResponse) -> Result<Response<Body>> {
    let (framing, payload) = unary_payload(&response.body)?;
    let mut message = BootstrapStatsigResponse::decode(payload)?;
    let mut config = serde_json::from_str::<Value>(&message.config)?;
    configure_agent_gates(&mut config)?;
    message.config = serde_json::to_string(&config)?;
    Ok(response.with_body(super::encode_unary(message.encode_to_vec(), framing)?))
}

fn local_response() -> Result<Response<Body>> {
    let generated_at_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or_default();
    let mut config = json!({
        "feature_gates": {},
        "dynamic_configs": {},
        "layer_configs": {},
        "user": {
            "userID": "local_ultra",
            "customIDs": { "localUserID": "local_ultra" }
        },
        "has_updates": true,
        "hash_used": "none",
        "sdkParams": {
            "stableID": "local_ultra",
            "disableDiagnosticsLogging": true
        },
        "time": generated_at_ms
    });
    configure_agent_gates(&mut config)?;
    let message = BootstrapStatsigResponse {
        config: serde_json::to_string(&config)?,
        generated_at_ms,
    };
    let body = message.encode_to_vec();
    let length = body.len();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    Ok(response)
}

fn configure_agent_gates(config: &mut Value) -> Result<()> {
    let overrides = AGENT_GATES.map(|(name, enabled)| (statsig_key(config, name), enabled));
    let root = config
        .as_object_mut()
        .ok_or_else(|| Error::Protocol("Statsig bootstrap config must be an object".into()))?;
    let gates = root
        .entry("feature_gates")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| Error::Protocol("Statsig feature_gates must be an object".into()))?;
    for (key, enabled) in overrides {
        gates.insert(key.clone(), feature_gate(&key, Enabled::from(enabled)));
    }
    Ok(())
}

fn statsig_key(config: &Value, name: &str) -> String {
    match config.get("hash_used").and_then(Value::as_str) {
        Some("djb2") => djb2(name),
        Some("sha256") => STANDARD.encode(Sha256::digest(name.as_bytes())),
        _ => name.to_owned(),
    }
}

fn djb2(value: &str) -> String {
    value
        .encode_utf16()
        .fold(0_u32, |hash, character| {
            hash.wrapping_mul(31).wrapping_add(u32::from(character))
        })
        .to_string()
}

fn feature_gate(name: &str, enabled: Enabled) -> Value {
    let rule = match enabled {
        Enabled::Enabled => "local_enabled",
        Enabled::Disabled => "local_disabled",
    };
    let enabled = enabled == Enabled::Enabled;
    json!({
        "name": name,
        "value": enabled,
        "rule_id": rule,
        "ruleID": rule,
        "group_name": rule,
        "groupName": rule,
        "secondary_exposures": [],
        "secondaryExposures": [],
        "undelegated_secondary_exposures": [],
        "undelegatedSecondaryExposures": [],
        "is_device_based": false,
        "isDeviceBased": false,
        "id_type": "userID",
        "idType": "userID"
    })
}

fn unary_payload(body: &Bytes) -> Result<(super::Framing, &[u8])> {
    super::unary_payload(body, "Statsig")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_bootstrap_enables_the_local_loop_gate() {
        let mut config = json!({ "feature_gates": {}, "hash_used": "none" });
        configure_agent_gates(&mut config).unwrap();
        assert_eq!(
            config["feature_gates"]["agent_host_local_loop"]["value"],
            true
        );
    }
}
