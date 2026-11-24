//! Keeps Cursor Agent traffic on HTTP/1.1 Connect.

use axum::{body::Body, http::Response};
use prost::Message;

const HTTP2_CONFIG_FORCE_ALL_DISABLED: i32 = 1;

#[derive(Clone, PartialEq, Message)]
struct ServerConfigResponse {
    #[prost(string, tag = "6")]
    config_version: String,
    #[prost(int32, tag = "7")]
    http2_config: i32,
    #[prost(bool, optional, tag = "28")]
    cli_sandbox_default_enabled: Option<bool>,
}

pub async fn get() -> Response<Body> {
    super::proto_response(&server_config())
}

fn server_config() -> ServerConfigResponse {
    ServerConfigResponse {
        config_version: "cursor_byok_next_local_agent_v1".into(),
        http2_config: HTTP2_CONFIG_FORCE_ALL_DISABLED,
        cli_sandbox_default_enabled: Some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forces_agent_cli_to_use_the_selected_legacy_endpoint() {
        let config = server_config();
        assert_eq!(config.http2_config, HTTP2_CONFIG_FORCE_ALL_DISABLED);
        assert_eq!(config.cli_sandbox_default_enabled, Some(true));
    }
}
