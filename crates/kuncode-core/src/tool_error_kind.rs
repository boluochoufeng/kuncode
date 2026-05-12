//! `ToolErrorKind`: the closed, wire-stable classification for tool failures.
//!
//! `kuncode-tools::ToolError` (Phase 2) carries the in-memory error with
//! diagnostics; its discriminant maps 1:1 to a `ToolErrorKind`, which is what
//! lands in `tool.failed` event payloads and any future provider response. See
//! `docs/plans/kuncode-phase2-tool-runtime-plan.md` §6.1 and §9.5.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorKind {
    UnknownTool,
    InvalidInput,
    CapabilityDenied,
    Workspace,
    Io,
    Process,
    Timeout,
    Cancelled,
    Artifact,
    ResultTooLarge,
    Internal,
}

#[cfg(test)]
mod tests {
    use super::ToolErrorKind;

    const WIRE: &[(ToolErrorKind, &str)] = &[
        (ToolErrorKind::UnknownTool, "unknown_tool"),
        (ToolErrorKind::InvalidInput, "invalid_input"),
        (ToolErrorKind::CapabilityDenied, "capability_denied"),
        (ToolErrorKind::Workspace, "workspace"),
        (ToolErrorKind::Io, "io"),
        (ToolErrorKind::Process, "process"),
        (ToolErrorKind::Timeout, "timeout"),
        (ToolErrorKind::Cancelled, "cancelled"),
        (ToolErrorKind::Artifact, "artifact"),
        (ToolErrorKind::ResultTooLarge, "result_too_large"),
        (ToolErrorKind::Internal, "internal"),
    ];

    #[test]
    fn serializes_to_snake_case() {
        for (kind, wire) in WIRE {
            let actual = serde_json::to_string(kind).expect("serialize");
            assert_eq!(actual, format!("\"{wire}\""), "{kind:?} should serialize to {wire}");
        }
    }

    #[test]
    fn deserializes_from_snake_case() {
        for (kind, wire) in WIRE {
            let actual: ToolErrorKind = serde_json::from_str(&format!("\"{wire}\"")).expect("deserialize");
            assert_eq!(actual, *kind, "{wire} should deserialize to {kind:?}");
        }
    }

    #[test]
    fn rejects_unknown_or_non_snake_case_variant() {
        assert!(serde_json::from_str::<ToolErrorKind>("\"network\"").is_err());
        assert!(serde_json::from_str::<ToolErrorKind>("\"UnknownTool\"").is_err());
        assert!(serde_json::from_str::<ToolErrorKind>("\"\"").is_err());
    }
}
