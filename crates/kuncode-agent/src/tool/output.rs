//! Model-visible tool output envelopes and harness-only retention metadata.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

const MAX_TOOL_ERROR_BYTES: usize = 2_048;

/// Harness-level failures the agent loop must handle itself.
#[derive(Debug, Error)]
pub enum ToolError {
    /// The tool was cancelled before completing.
    #[error("tool execution was cancelled")]
    Cancelled,
    /// A typed output failed at the harness boundary.
    #[error("internal tool error: {0}")]
    Internal(String),
}

/// Uniform envelope returned by every tool.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ToolOutput<D = serde_json::Value> {
    /// Whether the tool completed its requested operation.
    pub ok: bool,
    /// Typed success payload omitted on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<D>,
    /// Model-recoverable failure omitted on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolErrorPayload>,
    /// Whether the payload already omits unknown source content.
    pub truncated: bool,
}

/// Harness-owned retention policy for a completed tool result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolResultRetention {
    /// Preserve the provider-visible result unless semantic summarization absorbs it.
    #[default]
    Verbatim,
    /// Permit bounded deterministic projection outside the protected recent tail.
    Slimmable,
}

/// Stable wire category for a model-recoverable tool failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolErrorKind {
    /// Arguments failed to parse or validate before the tool ran.
    InvalidArguments,
    /// No tool with the requested name is registered.
    ToolNotFound,
    /// Blocked by a permission rule at the gate.
    PermissionDenied,
    /// The call was interrupted before the tool returned.
    Cancelled,
    /// A harness-boundary tool failure.
    ToolError,
    /// A tool-defined kind outside the harness vocabulary.
    Other(String),
}

impl ToolErrorKind {
    /// Returns the stable model-facing wire value.
    pub fn as_str(&self) -> &str {
        match self {
            Self::InvalidArguments => "invalid_arguments",
            Self::ToolNotFound => "tool_not_found",
            Self::PermissionDenied => "permission_denied",
            Self::Cancelled => "cancelled",
            Self::ToolError => "tool_error",
            Self::Other(kind) => kind,
        }
    }
}

impl From<&str> for ToolErrorKind {
    fn from(kind: &str) -> Self {
        match kind {
            "invalid_arguments" => Self::InvalidArguments,
            "tool_not_found" => Self::ToolNotFound,
            "permission_denied" => Self::PermissionDenied,
            "cancelled" => Self::Cancelled,
            "tool_error" => Self::ToolError,
            other => Self::Other(other.to_string()),
        }
    }
}

impl From<String> for ToolErrorKind {
    fn from(kind: String) -> Self {
        match Self::from(kind.as_str()) {
            Self::Other(_) => Self::Other(kind),
            known => known,
        }
    }
}

impl std::fmt::Display for ToolErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ToolErrorKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ToolErrorKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from(String::deserialize(deserializer)?))
    }
}

/// Model-visible detail for a recoverable tool failure.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolErrorPayload {
    /// Stable failure category used by the runner and frontends.
    pub kind: ToolErrorKind,
    /// Bounded diagnostic text intended for model recovery.
    pub message: String,
}

impl<D> ToolOutput<D> {
    /// Creates a complete successful output.
    pub fn success(data: D) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
            truncated: false,
        }
    }

    /// Creates a model-recoverable failed output.
    pub fn failure(kind: impl Into<ToolErrorKind>, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(ToolErrorPayload {
                kind: kind.into(),
                message: bounded_error_message(message.into()),
            }),
            truncated: false,
        }
    }

    /// Marks an output whose source content was already bounded.
    pub fn truncated(mut self) -> Self {
        self.truncated = true;
        self
    }
}

fn bounded_error_message(message: String) -> String {
    let mut bounded = String::with_capacity(message.len().min(MAX_TOOL_ERROR_BYTES));
    for character in message.chars() {
        let character = if character.is_control() && character != '\n' && character != '\t' {
            ' '
        } else {
            character
        };
        if bounded.len() + character.len_utf8() > MAX_TOOL_ERROR_BYTES {
            break;
        }
        bounded.push(character);
    }
    bounded
}

impl<D: Serialize> ToolOutput<D> {
    /// Erases typed data at the dynamic-dispatch boundary.
    ///
    /// # Errors
    /// Returns [`ToolError::Internal`] when typed data cannot become JSON.
    pub fn erase(self) -> Result<ToolOutput, ToolError> {
        let data = match self.data {
            Some(payload) => Some(serde_json::to_value(payload).map_err(|err| {
                ToolError::Internal(format!("failed to serialize tool output: {err}"))
            })?),
            None => None,
        };

        Ok(ToolOutput {
            ok: self.ok,
            data,
            error: self.error,
            truncated: self.truncated,
        })
    }

    /// Serializes the envelope for a provider-visible tool result.
    pub fn to_model_content(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|err| {
            serde_json::json!({
                "ok": false,
                "error": {
                    "kind": "serialization",
                    "message": format!("failed to serialize tool output: {err}")
                }
            })
            .to_string()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_messages_are_bounded_and_strip_terminal_controls() {
        let output: ToolOutput<()> = ToolOutput::failure(
            "test",
            format!("secret\u{1b}[31m{}", "🙂".repeat(MAX_TOOL_ERROR_BYTES)),
        );
        let message = output.error.expect("error exists").message;

        assert!(message.len() <= MAX_TOOL_ERROR_BYTES);
        assert!(message.len() >= MAX_TOOL_ERROR_BYTES - 3);
        assert!(!message.contains('\u{1b}'));
    }
}
