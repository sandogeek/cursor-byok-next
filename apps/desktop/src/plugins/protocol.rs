//! Defines the JSON session frames exchanged with a plugin VM.
//! Legacy capability frames remain parseable for diagnostics, never executable.

use serde::{Deserialize, Serialize};

/// One frame the host writes to a worker's stdin.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    /// Starts an invocation. The worker answers with events and one `result`.
    Request {
        id: String,
        method: String,
        params: serde_json::Value,
    },
    /// Withdraws an in-flight request; the worker should stop and answer with
    /// a cancelled result.
    Cancel { id: String },
}

/// One frame a worker writes to stdout.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMessage {
    /// The terminal answer for a request. `error` is set when the plugin
    /// reported a failure; `result` is then meaningless.
    Result {
        id: String,
        #[serde(default)]
        result: serde_json::Value,
        #[serde(default)]
        error: Option<String>,
    },
    /// One streamed model event for an in-flight request.
    Event {
        id: String,
        event: serde_json::Value,
    },
    /// A capability call the worker makes back into the host. `request_id`
    /// attributes it to the invocation that is allowed to make it.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "legacy frames are parsed only for diagnostics and never executed"
        )
    )]
    HostCall {
        id: String,
        #[serde(rename = "requestId")]
        request_id: String,
        method: String,
        #[serde(default)]
        params: serde_json::Value,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiplexed_host_call() {
        let message: WorkerMessage = serde_json::from_value(serde_json::json!({
            "type": "host_call",
            "id": "host-2",
            "requestId": "request-1",
            "method": "network.fetch",
            "params": { "url": "https://example.com" }
        }))
        .unwrap();
        match message {
            WorkerMessage::HostCall {
                id,
                request_id,
                method,
                params,
            } => {
                assert_eq!(id, "host-2");
                assert_eq!(request_id, "request-1");
                assert_eq!(method, "network.fetch");
                assert_eq!(params["url"], "https://example.com");
            }
            _ => panic!("expected host call"),
        }
    }

    #[test]
    fn golden_request_and_cancel_bytes() {
        let request = HostMessage::Request {
            id: "r1".into(),
            method: "models.list".into(),
            params: serde_json::json!({"providerId":"magpie"}),
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"type":"request","id":"r1","method":"models.list","params":{"providerId":"magpie"}}"#
        );
        assert_eq!(
            serde_json::to_string(&HostMessage::Cancel { id: "r1".into() }).unwrap(),
            r#"{"type":"cancel","id":"r1"}"#
        );
    }
}
