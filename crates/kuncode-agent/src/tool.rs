//! Tool interface exposed by the agent runtime.

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use tokio_util::sync::CancellationToken;

use crate::permission::PermissionRequest;
use crate::todo::TodoHandle;

pub mod bash;
pub mod filesystem;
pub mod todo_write;

mod output;

pub use output::{ToolError, ToolErrorKind, ToolErrorPayload, ToolOutput, ToolResultRetention};

/// Per-call execution context threaded into every tool.
///
/// Kept deliberately small. The permission *gate* lives in the runner, not here
/// (the runner computes the verdict before dispatch), and tools already self-hold
/// their [`Workspace`], so neither is duplicated here. It carries only the
/// per-session seams a tool genuinely needs at call time: a cancellation token,
/// and the [`TodoHandle`] for the session plan. Future fields (a
/// `request_permission` hook for subagents, a `cwd` override) attach here too.
///
/// [`Workspace`]: crate::workspace::Workspace
#[derive(Clone, Debug, Default)]
pub struct ToolContext {
    /// Cancelled by the runner (user interrupt / shutdown). A tool may observe
    /// it for cooperative cancellation, but the runner also races `call`
    /// against it, so most tools can ignore it.
    pub cancel: CancellationToken,
    /// Handle to the current session's task plan. The runner clones in the
    /// session's handle; `todo_write` writes through it. Defaults to a standalone
    /// empty handle, so a tool that ignores the plan — and tests — still gets a
    /// valid (if unobserved) target.
    pub todos: TodoHandle,
}

impl ToolContext {
    /// A context with a fresh, never-cancelled token and a standalone plan.
    /// Handy for tests and non-interactive callers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a context wrapping an existing cancellation token.
    pub fn with_cancel(cancel: CancellationToken) -> Self {
        Self {
            cancel,
            ..Self::default()
        }
    }

    /// Attaches the session's plan handle, so `todo_write` writes the session's
    /// plan rather than the standalone default.
    pub fn with_todos(mut self, todos: TodoHandle) -> Self {
        self.todos = todos;
        self
    }
}

/// Object-safe tool interface used to register and dispatch tools.
///
/// Most tools should implement [`TypedTool`] instead — it provides this trait
/// automatically with strongly-typed arguments and output and an
/// auto-generated JSON Schema. Implement `Tool` directly only when the
/// arguments are genuinely dynamic or the schema has to be hand-rolled.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Definition (name, description, argument schema) advertised to the model.
    fn definition(&self) -> &ToolDefinition;

    /// Tool name as the model sees it. Defaults to the definition's name so
    /// there is a single source of truth.
    fn name(&self) -> &str {
        &self.definition().name
    }

    /// Computes the permission request for a call, *before* it runs.
    ///
    /// Parses and lexically inspects the arguments only — no filesystem access.
    /// A parse failure returns `Err(ToolOutput)` (an `invalid_arguments`
    /// failure) so bad arguments are reported to the model and **never reach the
    /// approver**.
    fn permission(
        &self,
        args: &serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<PermissionRequest, ToolOutput>;

    /// Invoke the tool with the raw JSON arguments produced by the model.
    ///
    /// Two error channels: failures the model can react to (bad arguments,
    /// non-zero exit, …) are reported in [`ToolOutput`] so the loop can feed
    /// them back for a retry, while [`ToolError`] is for harness-level failures
    /// (cancellation, internal bugs) the loop handles itself.
    async fn call(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError>;

    /// Authorizes deterministic compaction for a successfully completed call.
    ///
    /// The default is fail-safe because tool names and model-supplied arguments
    /// alone cannot prove that exact output is no longer required.
    fn result_retention(
        &self,
        _args: &serde_json::Value,
        _output: &ToolOutput,
    ) -> ToolResultRetention {
        ToolResultRetention::Verbatim
    }
}

/// Ergonomic tool definition with strongly-typed arguments and output.
///
/// Implementors declare an [`Args`](TypedTool::Args) type — the argument schema
/// in the definition is generated from it (see [`definition_for`]), so the
/// schema sent to the model and the type the tool deserializes can never drift
/// apart — and an [`Output`](TypedTool::Output) payload type. A blanket impl
/// wires this up to [`Tool`], parsing arguments and erasing the typed output to
/// JSON once so individual tools never touch a raw [`serde_json::Value`].
///
/// Tools with a genuinely dynamic payload can set `type Output =
/// serde_json::Value`.
#[async_trait]
pub trait TypedTool: Send + Sync {
    /// Deserializable, schema-describable argument type for this tool.
    type Args: DeserializeOwned + JsonSchema + Send;

    /// Serializable `data` payload returned to the model — and available to
    /// Rust callers (sub-agents, todo, …) without re-parsing JSON.
    type Output: Serialize + Send;

    /// Cached definition. Build it once (e.g. in the constructor) with
    /// [`definition_for`] so schema generation isn't repeated per call.
    fn definition(&self) -> &ToolDefinition;

    /// Declares what this call wants to do, for the permission gate.
    ///
    /// Synchronous and **lexical only**: it may normalize a path string for
    /// rule matching, but must not touch the filesystem (no `canonicalize`).
    /// The real workspace-boundary check stays in [`run`](Self::run) — rules
    /// are policy, the boundary is a security invariant, and the two are
    /// separate layers. Required (no default) so every new tool consciously
    /// declares its permission surface instead of being silently misclassified.
    fn permission(&self, args: &Self::Args, ctx: &ToolContext) -> PermissionRequest;

    /// Run the tool with already-parsed, validated arguments.
    async fn run(&self, args: Self::Args, ctx: &ToolContext) -> ToolOutput<Self::Output>;

    /// Authorizes deterministic compaction after the typed output is erased.
    ///
    /// Override only when bounded projection cannot hide evidence needed to
    /// reason about mutable or external state.
    fn result_retention(
        &self,
        _args: &serde_json::Value,
        _output: &ToolOutput,
    ) -> ToolResultRetention {
        ToolResultRetention::Verbatim
    }
}

#[async_trait]
impl<T> Tool for T
where
    T: TypedTool,
{
    fn definition(&self) -> &ToolDefinition {
        TypedTool::definition(self)
    }

    fn permission(
        &self,
        args: &serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<PermissionRequest, ToolOutput> {
        // Parse here to build the request (and so bad arguments fail as
        // `invalid_arguments` before reaching the approver); `call` parses again
        // to run. The two parses are deliberate, not a missed optimization:
        // `permission` is a standalone `raw → request` projection, so the raw
        // JSON stays the single currency flowing through gate → (future) hook →
        // decide. A future `PreToolUse` hook that rewrites arguments can just
        // re-run this projection on the new raw and re-gate on it. Carrying a
        // single parsed value across the decision (the abandoned
        // `PreparedToolCall` direction) would instead go stale on every rewrite
        // and have to be rebuilt — fighting that grain. Args are tiny, so the
        // extra parse is nanoseconds.
        match serde_json::from_value::<T::Args>(args.clone()) {
            Ok(parsed) => Ok(TypedTool::permission(self, &parsed, ctx)),
            Err(err) => Err(ToolOutput::failure(
                ToolErrorKind::InvalidArguments,
                format!("failed to parse arguments: {err}"),
            )),
        }
    }

    async fn call(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let output = match serde_json::from_value::<T::Args>(args) {
            Ok(args) => self.run(args, ctx).await,
            Err(err) => {
                return Ok(ToolOutput::failure(
                    "invalid_arguments",
                    format!("failed to parse arguments: {err}"),
                ));
            }
        };

        output.erase()
    }

    fn result_retention(
        &self,
        args: &serde_json::Value,
        output: &ToolOutput,
    ) -> ToolResultRetention {
        TypedTool::result_retention(self, args, output)
    }
}

/// Build a [`ToolDefinition`] whose argument schema is generated from `A` via
/// `schemars`, keeping the advertised schema in lockstep with the type the
/// tool actually deserializes.
pub fn definition_for<A: JsonSchema>(
    name: impl Into<String>,
    description: impl Into<String>,
) -> ToolDefinition {
    let mut parameters =
        serde_json::to_value(schemars::schema_for!(A)).expect("JSON Schema serializes to a value");

    // Drop meta keys that function-calling APIs neither need nor expect.
    if let Some(obj) = parameters.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
    }

    ToolDefinition {
        name: name.into(),
        description: description.into(),
        parameters,
    }
}
