//! Tool protocol, runtime and built-in tools. See Phase 2 plan.
//!
//! `lib.rs` only declares modules and re-exports the protocol surface; all
//! implementation lives in the per-concern files (`descriptor.rs`,
//! `result.rs`, `runtime.rs`, `tools/*.rs`).

use async_trait::async_trait;
use kuncode_core::RiskFlag;

pub mod capability;
pub mod context;
pub mod descriptor;
pub mod error;
pub mod input;
pub mod result;
pub mod runtime;
pub mod schema;
pub mod tools;

pub use capability::is_allowed;
pub use context::{ToolContext, ToolLimits};
pub use descriptor::{DescriptorError, ToolDescriptor};
pub use error::ToolError;
pub use input::ToolInput;
pub use result::{SUMMARY_MAX_CHARS, ToolResult, ToolResultError};
pub use runtime::{RegisterError, ToolRuntime};
pub use schema::{CompiledSchema, SchemaError};
pub use tools::{
    ApplyPatchTool, ExecArgvTool, GitDiffTool, GitStatusTool, ReadFileTool, SearchTool, WriteFileTool, builtin_tools,
    register_builtin_tools,
};

/// Implemented by every concrete tool. The runtime calls `descriptor` once at
/// registration and `execute` per `ToolInput`. Tools must not emit lifecycle
/// events themselves — that is `ToolRuntime`'s job (plan §9.1).
#[async_trait]
pub trait Tool: Send + Sync {
    /// Static metadata. Returned by reference because the runtime caches it at
    /// registration; the value must not change across calls.
    fn descriptor(&self) -> &ToolDescriptor;

    /// Risk flags recorded into `tool.started` for this invocation.
    ///
    /// Most tools return their descriptor's static flags. Tools with
    /// per-input risk, such as `exec_argv`, may override this hook after the
    /// runtime has validated the input schema but before it emits
    /// `tool.started`.
    fn risk_flags(&self, _input: &ToolInput) -> Vec<RiskFlag> {
        self.descriptor().risk_flags.clone()
    }

    /// Execute one call. The implementer must **not**:
    ///
    /// 1. Re-validate the schema — runtime already did.
    /// 2. Re-check capabilities — runtime gates before reaching here.
    /// 3. Emit `tool.started` / `tool.completed` / `tool.failed` /
    ///    `tool.cancelled` — runtime owns the lifecycle envelope.
    /// 4. Touch the filesystem outside `ctx.workspace` / `ctx.lane`.
    ///
    /// The implementer **must**:
    ///
    /// 1. Honor `ctx.cancel_token` and `ctx.limits`.
    /// 2. Use `ctx.source_event_id` as the `source_event_id` for any artifact
    ///    saved during this call.
    /// 3. Keep `ToolResult.summary` within `SUMMARY_MAX_CHARS`.
    async fn execute(&self, input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError>;
}
