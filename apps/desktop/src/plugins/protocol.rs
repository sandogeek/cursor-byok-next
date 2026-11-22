//! Defines newline-delimited messages exchanged with a plugin worker.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage<'a> {
    Request {
        id: &'a str,
        method: &'a str,
        params: &'a serde_json::Value,
    },
    Cancel {
        id: &'a str,
    },
    HostResult {
        id: &'a str,
        result: &'a serde_json::Value,
    },
    HostError {
        id: &'a str,
        error: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMessage {
    Result {
        id: String,
        #[serde(default)]
        result: serde_json::Value,
        #[serde(default)]
        error: Option<String>,
    },
    Event {
        id: String,
        event: serde_json::Value,
    },
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
