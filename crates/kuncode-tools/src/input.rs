//! `ToolInput`: the runtime call boundary for a tool invocation.
//!
//! Phase 2 deliberately does not introduce a richer `ToolRequest` domain
//! model; `ToolInput` is just the (`request_id`, `name`, `payload`) triple the
//! runtime hands to `Tool::execute`. See Phase 2 plan §9.3.

use kuncode_core::ToolRequestId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolInput {
    /// Stable identifier for this invocation. Propagated into every
    /// `tool.started` / `tool.completed` / `tool.failed` / `tool.cancelled`
    /// envelope so that the four-event lifecycle of one call is joinable.
    pub request_id: ToolRequestId,

    /// Routing key into `ToolRuntime`. Must equal a registered
    /// `ToolDescriptor.name`; otherwise the runtime returns
    /// `ToolError::UnknownTool` before dispatch.
    pub name: String,

    /// Raw input payload. Validated against the matching descriptor's
    /// `input_schema` before the tool sees it.
    pub payload: Value,
}

impl ToolInput {
    pub fn new(name: impl Into<String>, payload: Value) -> Self {
        Self { request_id: ToolRequestId::new(), name: name.into(), payload }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_auto_generates_unique_request_ids() {
        let a = ToolInput::new("read_file", json!({ "path": "src/lib.rs" }));
        let b = ToolInput::new("read_file", json!({ "path": "src/lib.rs" }));
        assert_ne!(a.request_id, b.request_id);
        assert_eq!(a.name, "read_file");
    }

    #[test]
    fn round_trips_through_json() {
        let input = ToolInput::new("write_file", json!({ "path": "x", "content": "y" }));
        let raw = serde_json::to_string(&input).expect("serialize");
        let back: ToolInput = serde_json::from_str(&raw).expect("deserialize");
        assert_eq!(input, back);
    }
}
