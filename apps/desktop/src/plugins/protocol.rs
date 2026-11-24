//! Defines newline-delimited messages exchanged with a plugin worker.
//!
//! Host → worker: [`HostMessage`] frames, one JSON object per line. Worker →
//! host: [`WorkerMessage`] frames, same framing. A worker may issue
//! `host_call` frames at any time while its invocation is registered; every
//! such call must be answered with exactly one `host_result` or `host_error`
//! carrying the same id.

use serde::{Deserialize, Serialize};

/// One frame the host writes to a worker's stdin.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage<'a> {
    /// Starts an invocation. The worker answers with events and one `result`.
    Request {
        id: &'a str,
        method: &'a str,
        params: &'a serde_json::Value,
    },
    /// Withdraws an in-flight request; the worker should stop and answer with
    /// a cancelled result.
    Cancel { id: &'a str },
    /// The answer to a worker's `host_call`.
    HostResult {
        id: &'a str,
        result: &'a serde_json::Value,
    },
    /// The failed answer to a worker's `host_call`. `status` carries the HTTP
    /// status when the failure was an upstream response, so a plugin can tell
    /// an auth failure from a transport failure.
    HostError {
        id: &'a str,
        error: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
    },
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
                ..
            } => {
                assert_eq!(id, "host-2");
                assert_eq!(request_id, "request-1");
                assert_eq!(method, "network.fetch");
            }
            _ => panic!("expected host call"),
        }
    }

    #[test]
    fn serializes_host_error_status_only_when_present() {
        let with_status = serde_json::to_value(&HostMessage::HostError {
            id: "host-1",
            error: "HTTP 401: unauthorized",
            status: Some(401),
        })
        .unwrap();
        assert_eq!(with_status["type"], "host_error");
        assert_eq!(with_status["status"], 401);
        let without_status = serde_json::to_value(&HostMessage::HostError {
            id: "host-1",
            error: "plugin host call failed",
            status: None,
        })
        .unwrap();
        assert!(without_status.get("status").is_none());
    }
}
