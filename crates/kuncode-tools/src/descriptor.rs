//! `ToolDescriptor`: the static metadata a tool advertises at registration.
//!
//! Structural invariants validated by [`ToolDescriptor::validate`]
//! (plan §9.2 items 1-4):
//!
//! 1. `name` non-empty.
//! 2. `description` non-empty.
//! 3. `effects` non-empty.
//! 4. `default_capabilities` non-empty.
//!
//! The schema-compilability check (§9.2 item 5) is owned by
//! `CompiledSchema::compile` and enforced by `ToolRuntime::register`.

use kuncode_core::{RiskFlag, ToolCapability, ToolEffect};
use serde_json::Value;
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct ToolDescriptor {
    /// Unique routing key inside a `ToolRuntime`. Must match `ToolInput.name`
    /// when the runtime dispatches. Non-empty; registration rejects duplicates.
    pub name: String,

    /// Natural-language description handed to the LLM via the Phase 3 render
    /// layer. Non-empty. Treat as part of the model-facing contract — changes
    /// to wording can shift tool-selection behavior.
    pub description: String,

    /// JSON Schema validating `ToolInput.payload`. The runtime compiles it
    /// once at registration and caches the result; compile failure surfaces as
    /// `RegisterError::Schema`. Per-call validation failure becomes
    /// `ToolError::InvalidInput`.
    pub input_schema: Value,

    /// Optional output schema, advisory only. Used by Phase 3 to help providers
    /// render structured-output hints; the runtime does **not** validate
    /// `ToolResult.metadata` against it.
    pub output_schema: Option<Value>,

    /// What this tool *can do* — static side-effect surface. Recorded into
    /// every `tool.started` envelope so the event log is self-describing. Must
    /// be non-empty; an effect-free tool would be a no-op.
    pub effects: Vec<ToolEffect>,

    /// Who is allowed to call this tool. The single source of truth read by
    /// **two** layers:
    ///
    /// 1. Phase 3 render layer — filters which tools appear in the LLM prompt.
    /// 2. `ToolRuntime` execute gate — denies calls whose granted capabilities
    ///    do not intersect this list (`is_allowed`).
    ///
    /// Non-empty; an empty list would make the tool unreachable.
    pub default_capabilities: Vec<ToolCapability>,

    /// Per-invocation risk labels attached automatically to every call of this
    /// tool. Phase 2 only records them into `tool.started`; Phase 5 policy
    /// reads them to decide Ask/Deny. May be empty.
    pub risk_flags: Vec<RiskFlag>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum DescriptorError {
    #[error("descriptor `name` must not be empty")]
    EmptyName,

    #[error("descriptor `description` must not be empty")]
    EmptyDescription,

    #[error("descriptor `effects` must not be empty")]
    EmptyEffects,

    #[error("descriptor `default_capabilities` must not be empty")]
    EmptyCapabilities,
}

impl ToolDescriptor {
    /// Validate the structural invariants of the descriptor (plan §9.2 items 1-4).
    ///
    /// JSON Schema compilability (§9.2 item 5) is **not** checked here — that
    /// is the concern of `CompiledSchema::compile` and is enforced separately
    /// by `ToolRuntime::register` as `RegisterError::Schema`.
    pub fn validate(&self) -> Result<(), DescriptorError> {
        if self.name.is_empty() {
            return Err(DescriptorError::EmptyName);
        }

        if self.description.is_empty() {
            return Err(DescriptorError::EmptyDescription);
        }

        if self.effects.is_empty() {
            return Err(DescriptorError::EmptyEffects);
        }

        if self.default_capabilities.is_empty() {
            return Err(DescriptorError::EmptyCapabilities);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_descriptor() -> ToolDescriptor {
        ToolDescriptor {
            name: "read_file".to_owned(),
            description: "Read a UTF-8 text file inside the workspace.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
            output_schema: None,
            effects: vec![ToolEffect::ReadWorkspace],
            default_capabilities: vec![ToolCapability::Explore],
            risk_flags: vec![],
        }
    }

    #[test]
    fn validate_accepts_minimal_descriptor() {
        valid_descriptor().validate().expect("valid descriptor");
    }

    #[test]
    fn validate_rejects_empty_name() {
        let mut d = valid_descriptor();
        d.name.clear();
        assert_eq!(d.validate(), Err(DescriptorError::EmptyName));
    }

    #[test]
    fn validate_rejects_empty_description() {
        let mut d = valid_descriptor();
        d.description.clear();
        assert_eq!(d.validate(), Err(DescriptorError::EmptyDescription));
    }

    #[test]
    fn validate_rejects_empty_effects() {
        let mut d = valid_descriptor();
        d.effects.clear();
        assert_eq!(d.validate(), Err(DescriptorError::EmptyEffects));
    }

    #[test]
    fn validate_rejects_empty_capabilities() {
        let mut d = valid_descriptor();
        d.default_capabilities.clear();
        assert_eq!(d.validate(), Err(DescriptorError::EmptyCapabilities));
    }

    #[test]
    fn validate_ignores_input_schema_shape() {
        // Schema validity is `CompiledSchema::compile`'s concern, not validate's.
        let mut d = valid_descriptor();
        d.input_schema = json!(42);
        d.validate().expect("structural validation must not look at the schema");
    }
}
