//! `ToolContext` and `ToolLimits`: the per-invocation runtime context handed
//! to `Tool::execute`. See plan §10.

use kuncode_core::{AgentId, EventId, RunId, TurnId};
use kuncode_events::{ArtifactStore, EventSinkHandle};
use kuncode_workspace::{ExecutionLane, Workspace};
use tokio_util::sync::CancellationToken;

/// Per-invocation, borrow-only context passed to `Tool::execute`.
///
/// Tools must not assume anything beyond what is exposed here. The
/// `source_event_id` always points to the `tool.started` envelope emitted by
/// the runtime so that any artifact saved during execution links back to the
/// triggering tool request.
pub struct ToolContext<'a> {
    /// The owning run. Used by the tool when it needs to scope artifacts or
    /// child events to the current run.
    pub run_id: RunId,

    /// The calling agent, when one exists. `None` for direct CLI / test paths
    /// that drive the runtime without an agent loop.
    pub agent_id: Option<AgentId>,

    /// The model turn that produced this `ToolInput`, when applicable. `None`
    /// for invocations outside an agent loop.
    pub turn_id: Option<TurnId>,

    /// `event_id` of the `tool.started` envelope the runtime emitted before
    /// calling `execute`. Any artifact created during this call MUST use this
    /// as `source_event_id` so traces resolve back to the triggering call.
    pub source_event_id: EventId,

    /// Path-safety boundary. Every file path the tool touches must be resolved
    /// through `Workspace`; raw `std::fs` access is a bug.
    pub workspace: &'a Workspace,

    /// Execution lane the tool runs in. MVP only provides `MainWorkspace`;
    /// `cwd` for any subprocess must live inside `lane.root_path()`.
    pub lane: &'a ExecutionLane,

    /// Handle for emitting *sub-events* (e.g. progress signals). Phase 2 base
    /// lifecycle events (`tool.started/completed/failed/cancelled`) are still
    /// emitted by `ToolRuntime`, not by the tool itself.
    pub event_sink: EventSinkHandle,

    /// Artifact persistence. `None` means the calling context opted out (some
    /// tests / one-shot CLI paths); tools that need to persist large output
    /// must surface `ToolError::Artifact` in that case rather than silently
    /// dropping data.
    pub artifact_store: Option<&'a (dyn ArtifactStore + Send + Sync)>,

    /// Cooperative cancellation. The tool must poll or `select!` on this and
    /// return `ToolError::Cancelled` promptly; the runtime maps it to
    /// `tool.cancelled` and reclaims subprocess resources.
    pub cancel_token: CancellationToken,

    /// Per-invocation byte/time caps the tool must honor.
    pub limits: ToolLimits,
}

/// Per-invocation byte / time caps. Defaults match plan §10.1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolLimits {
    /// Cap on bytes that may travel back inline in `ToolResult.inline_content`
    /// or be retained in memory before spilling to an artifact.
    pub max_inline_output_bytes: usize,

    /// Cap on captured stdout for subprocess tools. Stdout past this is
    /// truncated and spilled to an artifact via `content_ref`.
    pub max_stdout_bytes: usize,

    /// Cap on captured stderr for subprocess tools. Same overflow handling as
    /// stdout.
    pub max_stderr_bytes: usize,

    /// Timeout applied when the tool's input does not specify one.
    pub default_timeout_ms: u64,

    /// Hard ceiling on any caller-supplied timeout. Inputs exceeding this are
    /// clamped (or rejected as `ToolError::InvalidInput`, tool's choice).
    pub max_timeout_ms: u64,
}

impl ToolLimits {
    pub const DEFAULT_MAX_INLINE_OUTPUT_BYTES: usize = 32 * 1024;
    pub const DEFAULT_MAX_STDOUT_BYTES: usize = 256 * 1024;
    pub const DEFAULT_MAX_STDERR_BYTES: usize = 256 * 1024;
    pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
    pub const MAX_TIMEOUT_MS: u64 = 600_000;
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            max_inline_output_bytes: Self::DEFAULT_MAX_INLINE_OUTPUT_BYTES,
            max_stdout_bytes: Self::DEFAULT_MAX_STDOUT_BYTES,
            max_stderr_bytes: Self::DEFAULT_MAX_STDERR_BYTES,
            default_timeout_ms: Self::DEFAULT_TIMEOUT_MS,
            max_timeout_ms: Self::MAX_TIMEOUT_MS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_limits_match_plan_section_10_1() {
        let limits = ToolLimits::default();
        assert_eq!(limits.max_inline_output_bytes, 32 * 1024);
        assert_eq!(limits.max_stdout_bytes, 256 * 1024);
        assert_eq!(limits.max_stderr_bytes, 256 * 1024);
        assert_eq!(limits.default_timeout_ms, 120_000);
        assert_eq!(limits.max_timeout_ms, 600_000);
    }

    #[test]
    fn default_timeout_does_not_exceed_max_timeout() {
        let limits = ToolLimits::default();
        assert!(limits.default_timeout_ms <= limits.max_timeout_ms);
    }
}
