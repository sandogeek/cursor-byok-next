//! Configures Cursor feature gates so agent-host chooses the local loop.

use axum::{
    body::Body,
    extract::Extension,
    http::{Request, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use prost::Message;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::{Error, Result, backend::proxy, protocol};

pub const BOOTSTRAP_STATSIG_PATH: &str = "/aiserver.v1.AnalyticsService/BootstrapStatsig";
/// The gates agent-host reads to route a turn to the local loop. Every gate
/// here is forced on; a gate that must stay off does not belong in the list.
const AGENT_GATES: [&str; 2] = ["nal_agent_retries", "agent_host_local_loop"];

#[derive(Clone, PartialEq, Message)]
struct BootstrapStatsigResponse {
    #[prost(string, tag = "1")]
    config: String,
    #[prost(uint64, tag = "2")]
    generated_at_ms: u64,
}

/// Patches Cursor's Statsig bootstrap so the agent-host gates choose the local loop, falling back to a local bootstrap.
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
    let body = protocol::rewrite_unary(&response.body, "Statsig", |payload| {
        let mut message = BootstrapStatsigResponse::decode(payload)?;
        let mut config = serde_json::from_str::<Value>(&message.config)?;
        configure_agent_gates(&mut config)?;
        message.config = serde_json::to_string(&config)?;
        Ok(message.encode_to_vec())
    })?;
    Ok(response.with_body(body))
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
    Ok(super::proto_response(&message))
}

fn configure_agent_gates(config: &mut Value) -> Result<()> {
    let keys: Vec<String> = AGENT_GATES
        .iter()
        .map(|name| statsig_key(config, name))
        .collect();
    let root = config
        .as_object_mut()
        .ok_or_else(|| Error::Protocol("Statsig bootstrap config must be an object".into()))?;
    let gates = root
        .entry("feature_gates")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| Error::Protocol("Statsig feature_gates must be an object".into()))?;
    for key in keys {
        gates.insert(key.clone(), enabled_gate(&key));
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

fn enabled_gate(name: &str) -> Value {
    const RULE: &str = "local_enabled";
    json!({
        "name": name,
        "value": true,
        "rule_id": RULE,
        "ruleID": RULE,
        "group_name": RULE,
        "groupName": RULE,
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
