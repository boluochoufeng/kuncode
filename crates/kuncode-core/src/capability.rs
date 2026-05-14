//! `ToolCapability`: capability labels a runtime grants to an agent.
//!
//! Phase 2 capability gate rule: a tool is admitted iff its descriptor's
//! `default_capabilities` intersects the currently granted set. Full policy
//! (`RunMode`, Ask, profile) lands in Phase 5. See
//! `docs/plans/kuncode-phase2-tool-runtime-plan.md` §4.1 and §5.2.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCapability {
    Explore,
    Verify,
    Edit,
    Lead,
}

#[cfg(test)]
mod tests {
    use super::ToolCapability;
    use crate::effect::ToolEffect;

    #[test]
    fn serializes_to_snake_case() {
        let cases = [
            (ToolCapability::Explore, "\"explore\""),
            (ToolCapability::Verify, "\"verify\""),
            (ToolCapability::Edit, "\"edit\""),
            (ToolCapability::Lead, "\"lead\""),
        ];
        for (cap, expected) in cases {
            let actual = serde_json::to_string(&cap).expect("serialize");
            assert_eq!(actual, expected, "{cap:?} should serialize to {expected}");
        }
    }

    #[test]
    fn deserializes_from_snake_case() {
        let cases = [
            ("\"explore\"", ToolCapability::Explore),
            ("\"verify\"", ToolCapability::Verify),
            ("\"edit\"", ToolCapability::Edit),
            ("\"lead\"", ToolCapability::Lead),
        ];
        for (wire, expected) in cases {
            let actual: ToolCapability = serde_json::from_str(wire).expect("deserialize");
            assert_eq!(actual, expected, "{wire} should deserialize to {expected:?}");
        }
    }

    #[test]
    fn rejects_unknown_variant() {
        assert!(serde_json::from_str::<ToolCapability>("\"admin\"").is_err());
        assert!(serde_json::from_str::<ToolCapability>("\"Explore\"").is_err());
    }

    /// Truth table for the MVP built-in tool catalog. Source of truth:
    /// `docs/plans/kuncode-mvp-development-plan.md` §8. If this assertion
    /// drifts, update the spec first, then this test.
    #[test]
    fn mvp_tool_truth_table() {
        use ToolCapability::*;
        use ToolEffect::*;

        // The concrete built-in descriptors live in `kuncode-tools`, which
        // depends on this crate. Descriptor drift is checked from that crate's
        // integration tests to avoid a core -> tools dependency cycle.
        let table: &[(&str, &[ToolEffect], &[ToolCapability])] = &[
            ("read_file", &[ReadWorkspace], &[Explore, Edit]),
            ("search", &[ReadWorkspace], &[Explore, Edit]),
            ("write_file", &[WriteWorkspace], &[Edit]),
            ("apply_patch", &[WriteWorkspace], &[Edit]),
            ("exec_argv", &[ExecuteProcess], &[Verify, Edit]),
            ("git_status", &[ReadWorkspace], &[Explore, Verify]),
            ("git_diff", &[ReadWorkspace], &[Explore, Verify]),
            ("task_update", &[ModifyTaskBoard], &[Lead]),
        ];

        for (name, effects, caps) in table {
            assert!(!effects.is_empty(), "{name}: effects must be non-empty");
            assert!(!caps.is_empty(), "{name}: capabilities must be non-empty");
        }

        let (read_file_effects, read_file_caps) = (table[0].1, table[0].2);
        assert_eq!(read_file_effects, &[ReadWorkspace]);
        assert_eq!(read_file_caps, &[Explore, Edit]);

        let (exec_effects, exec_caps) = (table[4].1, table[4].2);
        assert_eq!(exec_effects, &[ExecuteProcess]);
        assert_eq!(exec_caps, &[Verify, Edit]);

        let (task_effects, task_caps) = (table[7].1, table[7].2);
        assert_eq!(task_effects, &[ModifyTaskBoard]);
        assert_eq!(task_caps, &[Lead]);
    }
}
