//! Tool interface exposed by the agent runtime.

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::permission::PermissionRequest;
use crate::todo::TodoHandle;

pub mod bash;
pub mod filesystem;
pub mod todo_write;

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

/// Harness-level failures the agent loop must handle itself — as opposed to
/// failures the model can react to, which are reported inside [`ToolOutput`].
///
/// [`ToolError::Cancelled`] is surfaced by the runner, which races
/// [`Tool::call`] against the [`ToolContext`] cancellation token (user
/// interrupt / shutdown) and maps a token win onto
/// [`AgentError::Cancelled`](crate::error::AgentError::Cancelled).
/// [`ToolError::Internal`] is produced when a tool's typed output fails to
/// serialize at the dispatch boundary.
#[derive(Debug, Error)]
pub enum ToolError {
    /// The tool was cancelled before completing (user interrupt or shutdown).
    #[error("tool execution was cancelled")]
    Cancelled,

    /// An internal invariant inside the tool runtime broke. This is a bug, not
    /// something the model can recover from.
    #[error("internal tool error: {0}")]
    Internal(String),
}

/// Uniform envelope returned by every tool.
///
/// `D` is the typed `data` payload. Tools work with a concrete `D` (e.g.
/// `ToolOutput<BashOutput>`); at the [`dyn Tool`](Tool) boundary it is erased
/// to `ToolOutput<serde_json::Value>` (the default) via [`ToolOutput::erase`].
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ToolOutput<D = serde_json::Value> {
    pub ok: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<D>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolErrorPayload>,

    pub truncated: bool,
}

/// The `kind` tag on a [`ToolErrorPayload`] (and its event mirror
/// [`ToolFailure`](crate::observer::ToolFailure)).
///
/// The harness produces a fixed protocol vocabulary — the named variants — that
/// the runner, gate, and frontends recognize. Tools may report their own
/// open-ended kinds (e.g. bash's `non_zero_exit`), which round-trip through
/// [`Other`](Self::Other). So the harness vocabulary is type-checked in one
/// place while the tool taxonomy stays open.
///
/// Serializes transparently to the snake_case wire string the model reads in its
/// `tool_result` and audit records — the variants change nothing on the wire.
/// [`From<&str>`](Self::from) canonicalizes a known string to its variant, so a
/// tool that passes `"permission_denied"` and the gate that passes
/// [`PermissionDenied`](Self::PermissionDenied) are indistinguishable downstream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolErrorKind {
    /// Arguments failed to parse or validate before the tool ran.
    InvalidArguments,
    /// No tool with the requested name is registered.
    UnknownTool,
    /// Blocked by a permission rule at the gate.
    PermissionDenied,
    /// Vetoed by a `PreToolUse` hook.
    BlockedByHook,
    /// The call was interrupted before the tool returned.
    Cancelled,
    /// A harness-boundary tool failure (e.g. output failed to serialize).
    ToolError,
    /// A tool-defined kind outside the harness vocabulary.
    Other(String),
}

impl ToolErrorKind {
    /// The wire / model-facing string for this kind.
    pub fn as_str(&self) -> &str {
        match self {
            Self::InvalidArguments => "invalid_arguments",
            Self::UnknownTool => "unknown_tool",
            Self::PermissionDenied => "permission_denied",
            Self::BlockedByHook => "blocked_by_hook",
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
            "unknown_tool" => Self::UnknownTool,
            "permission_denied" => Self::PermissionDenied,
            "blocked_by_hook" => Self::BlockedByHook,
            "cancelled" => Self::Cancelled,
            "tool_error" => Self::ToolError,
            other => Self::Other(other.to_string()),
        }
    }
}

impl From<String> for ToolErrorKind {
    fn from(kind: String) -> Self {
        // Reuse the &str table; only an unknown string keeps its allocation.
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

// Transparent (de)serialization: the wire form is just the kind string, so the
// model-facing `tool_result` and serialized events are unchanged by the typing.
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolErrorPayload {
    pub kind: ToolErrorKind,
    pub message: String,
}

impl<D> ToolOutput<D> {
    pub fn success(data: D) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
            truncated: false,
        }
    }

    pub fn failure(kind: impl Into<ToolErrorKind>, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(ToolErrorPayload {
                kind: kind.into(),
                message: message.into(),
            }),
            truncated: false,
        }
    }

    pub fn truncated(mut self) -> Self {
        self.truncated = true;
        self
    }
}

impl<D: Serialize> ToolOutput<D> {
    /// Erase the typed `data` payload to a [`serde_json::Value`] for the
    /// dynamic-dispatch boundary. A serialization failure is a tool bug, so it
    /// surfaces as [`ToolError::Internal`].
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
