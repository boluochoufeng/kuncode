//! Thin wrapper over the `jsonschema` crate so the rest of `kuncode-tools`
//! depends on a stable internal type. The compiled validator is cached inside
//! `ToolRuntime` per registered descriptor; see plan §11.1.

use jsonschema::Validator;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SchemaError {
    #[error("schema failed to compile: {message}")]
    Compile { message: String },

    #[error("instance does not validate: {message}")]
    Validate { message: String },
}

/// A compiled JSON Schema validator. Held by `ToolRuntime` and used to validate
/// each `ToolInput.payload` before dispatch.
#[derive(Debug)]
pub struct CompiledSchema {
    // Implementation detail; expose a method instead of the raw validator.
    validator: Validator,
}

impl CompiledSchema {
    /// Compile a JSON Schema value. The schema must be a JSON object — bare
    /// scalars are rejected as `SchemaError::Compile`.
    pub fn compile(schema: &Value) -> Result<Self, SchemaError> {
        let validator =
            jsonschema::validator_for(schema).map_err(|e| SchemaError::Compile { message: e.to_string() })?;
        Ok(Self { validator })
    }

    /// Validate a payload against this schema. Returns a single aggregated
    /// `SchemaError::Validate` on failure.
    pub fn validate(&self, instance: &Value) -> Result<(), SchemaError> {
        let errors: Vec<String> = self.validator.iter_errors(instance).map(|e| e.to_string()).collect();
        if errors.is_empty() { Ok(()) } else { Err(SchemaError::Validate { message: errors.join(";") }) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_schema() -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
            "additionalProperties": false,
        })
    }

    #[test]
    fn compile_accepts_object_schema() {
        CompiledSchema::compile(&sample_schema()).expect("compile");
    }

    #[test]
    fn compile_rejects_non_schema_value() {
        let err = CompiledSchema::compile(&json!(42)).expect_err("must reject");
        assert!(matches!(err, SchemaError::Compile { .. }));
    }

    #[test]
    fn validate_accepts_matching_payload() {
        let schema = CompiledSchema::compile(&sample_schema()).expect("compile");
        schema.validate(&json!({ "path": "src/lib.rs" })).expect("matching payload");
    }

    #[test]
    fn validate_rejects_missing_required_field() {
        let schema = CompiledSchema::compile(&sample_schema()).expect("compile");
        let err = schema.validate(&json!({})).expect_err("must reject");
        assert!(matches!(err, SchemaError::Validate { .. }));
    }

    #[test]
    fn validate_rejects_unknown_property() {
        let schema = CompiledSchema::compile(&sample_schema()).expect("compile");
        let err = schema.validate(&json!({ "path": "p", "extra": 1 })).expect_err("must reject");
        assert!(matches!(err, SchemaError::Validate { .. }));
    }
}
