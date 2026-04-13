// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

// Forked from src/agent/claude_cli/control.rs — keep in sync manually

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Control requests (sent to CLI via stdin)
// ---------------------------------------------------------------------------

/// Control request sent to the CLI subprocess via stdin.
#[derive(Debug, Clone, Serialize)]
pub struct ControlRequest {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub control: ControlPayload,
}

/// The control payload — determines what action to take.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ControlPayload {
    #[serde(rename = "interrupt")]
    Interrupt,

    #[serde(rename = "set_model")]
    SetModel { model: String },

    #[serde(rename = "permission_response")]
    PermissionResponse { id: String, granted: bool },
}

impl ControlRequest {
    /// Create an interrupt control request.
    pub fn interrupt() -> Self {
        Self {
            msg_type: "control",
            control: ControlPayload::Interrupt,
        }
    }

    /// Create a model switch control request.
    pub fn set_model(model: impl Into<String>) -> Self {
        Self {
            msg_type: "control",
            control: ControlPayload::SetModel {
                model: model.into(),
            },
        }
    }

    /// Create a permission response (grant or deny).
    pub fn permission_response(id: impl Into<String>, granted: bool) -> Self {
        Self {
            msg_type: "control",
            control: ControlPayload::PermissionResponse {
                id: id.into(),
                granted,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Control responses (received from CLI via stdout, inline in NDJSON stream)
// ---------------------------------------------------------------------------

/// A control request from the CLI (e.g. asking for permission).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponseMessage {
    pub id: String,
    #[serde(rename = "type")]
    pub ctrl_type: String,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_request_serialize() {
        let req = ControlRequest::interrupt();
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["type"], "control");
        assert_eq!(json["control"]["type"], "interrupt");
    }

    #[test]
    fn set_model_request_serialize() {
        let req = ControlRequest::set_model("claude-sonnet-4-5-20250929");
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["type"], "control");
        assert_eq!(json["control"]["type"], "set_model");
        assert_eq!(json["control"]["model"], "claude-sonnet-4-5-20250929");
    }

    #[test]
    fn permission_response_serialize_granted() {
        let req = ControlRequest::permission_response("perm-123", true);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["type"], "control");
        assert_eq!(json["control"]["type"], "permission_response");
        assert_eq!(json["control"]["id"], "perm-123");
        assert_eq!(json["control"]["granted"], true);
    }

    #[test]
    fn permission_response_serialize_denied() {
        let req = ControlRequest::permission_response("perm-456", false);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["control"]["granted"], false);
    }

    #[test]
    fn control_response_deserialize() {
        let json = r#"{"id":"r1","type":"permission_request","tool_name":"Bash","description":"Run command"}"#;
        let resp: ControlResponseMessage = serde_json::from_str(json).unwrap();
        assert_eq!(resp.id, "r1");
        assert_eq!(resp.ctrl_type, "permission_request");
        assert_eq!(resp.tool_name.as_deref(), Some("Bash"));
        assert_eq!(resp.description.as_deref(), Some("Run command"));
    }
}
