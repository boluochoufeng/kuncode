//! `ToolEffect`: the closed set of side effects a tool may declare.
//!
//! Effects describe *what a tool can do*; risk flags describe per-invocation
//! signals; capabilities describe *who is allowed to ask*. See
//! `docs/specs/kuncode-agent-harness-design.md` §6 and
//! `docs/plans/kuncode-phase2-tool-runtime-plan.md` §5.1.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    ReadWorkspace,
    WriteWorkspace,
    ExecuteProcess,
    ModifyTaskBoard,
}

#[cfg(test)]
mod tests {
    use super::ToolEffect;

    #[test]
    fn serializes_to_snake_case() {
        let cases = [
            (ToolEffect::ReadWorkspace, "\"read_workspace\""),
            (ToolEffect::WriteWorkspace, "\"write_workspace\""),
            (ToolEffect::ExecuteProcess, "\"execute_process\""),
            (ToolEffect::ModifyTaskBoard, "\"modify_task_board\""),
        ];
        for (effect, expected) in cases {
            let actual = serde_json::to_string(&effect).expect("serialize");
            assert_eq!(actual, expected, "{effect:?} should serialize to {expected}");
        }
    }

    #[test]
    fn deserializes_from_snake_case() {
        let cases = [
            ("\"read_workspace\"", ToolEffect::ReadWorkspace),
            ("\"write_workspace\"", ToolEffect::WriteWorkspace),
            ("\"execute_process\"", ToolEffect::ExecuteProcess),
            ("\"modify_task_board\"", ToolEffect::ModifyTaskBoard),
        ];
        for (wire, expected) in cases {
            let actual: ToolEffect = serde_json::from_str(wire).expect("deserialize");
            assert_eq!(actual, expected, "{wire} should deserialize to {expected:?}");
        }
    }

    #[test]
    fn rejects_unknown_variant() {
        assert!(serde_json::from_str::<ToolEffect>("\"network\"").is_err());
        assert!(serde_json::from_str::<ToolEffect>("\"ReadWorkspace\"").is_err());
    }
}
