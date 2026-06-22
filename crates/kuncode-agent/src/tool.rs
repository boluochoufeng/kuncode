//! Tool interface exposed by the agent runtime.

use std::any::Any;

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
    /// it for cooperative cancellation, but the runner also races `dispatch`
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
/// [`Tool::dispatch`] against the [`ToolContext`] cancellation token (user
/// interrupt / shutdown) and maps a token win onto
/// [`AgentError::Cancelled`](crate::error::AgentError::Cancelled).
/// [`ToolError::Internal`] is produced at the dispatch boundary when a tool's
/// typed output fails to serialize, or a prepared call is routed to the wrong
/// tool (see [`PreparedToolCall`]).
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

/// A parsed, gate-pending tool call: everything needed to run a tool exactly
/// once, produced by [`Tool::prepare`] and consumed by [`Tool::dispatch`].
///
/// Splitting preparation from dispatch lets the runner gate the call — and run
/// `PreToolUse` hooks — *between* the two without re-parsing the arguments.
///
/// # Invariants
///
/// - **Dispatch by the preparing tool only.** [`dispatch`](Tool::dispatch)
///   downcasts the erased arguments back to its own `Args`; routing a prepared
///   call to a *different* tool is a harness bug and fails with
///   [`ToolError::Internal`], not a model-recoverable error.
/// - **`parsed` is authoritative for execution; `raw` is advisory.** The tool
///   runs the parsed arguments, not [`raw`](Self::raw). `raw` is kept so
///   hooks/audit can read the model's wire form. If a future `PreToolUse` hook
///   ever *rewrites* arguments it must re-[`prepare`](Tool::prepare) the call —
///   mutating `raw` alone is silently ignored at dispatch.
/// - **Single-use.** The erased `parsed` payload is not `Clone`; a prepared
///   call is dispatched at most once.
pub struct PreparedToolCall {
    /// The permission request to gate on; also carries the human summary the
    /// runner shows in `ToolStart`.
    pub request: PermissionRequest,
    /// The model's original JSON arguments, retained for hooks and audit. Not
    /// the execution source of truth — see the type-level invariants.
    pub raw: serde_json::Value,
    /// Parsed `Args`, type-erased to ride through the `dyn Tool` boundary.
    /// Downcast back by the originating tool in [`dispatch`](Tool::dispatch).
    parsed: Box<dyn Any + Send>,
}

impl PreparedToolCall {
    /// Bundles a gate request with the raw + parsed arguments. `parsed` is the
    /// tool's own `Args`; the matching [`Tool::dispatch`] downcasts it back.
    pub fn new(
        request: PermissionRequest,
        raw: serde_json::Value,
        parsed: impl Any + Send,
    ) -> Self {
        Self {
            request,
            raw,
            parsed: Box::new(parsed),
        }
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

    /// Parses arguments into a [`PreparedToolCall`], *before* the call runs —
    /// the first half of the gated execution path.
    ///
    /// Parses and lexically inspects the arguments only — no filesystem access.
    /// A parse failure returns `Err(ToolOutput)` (an `invalid_arguments`
    /// failure) so bad arguments are reported to the model and **never reach the
    /// approver**. On success the returned call carries the request to gate on
    /// plus the parsed arguments, so [`dispatch`](Self::dispatch) needs no
    /// re-parse.
    fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<PreparedToolCall, ToolOutput>;

    /// Runs a call already parsed by [`prepare`](Self::prepare) — the second
    /// half, invoked after the gate allows it.
    ///
    /// Two error channels: failures the model can react to (non-zero exit, …)
    /// are reported in [`ToolOutput`] so the loop can feed them back for a
    /// retry, while [`ToolError`] is for harness-level failures (cancellation,
    /// internal bugs) the loop handles itself. A `prepared` built by a
    /// *different* tool fails with [`ToolError::Internal`] (see
    /// [`PreparedToolCall`] invariants).
    async fn dispatch(
        &self,
        prepared: PreparedToolCall,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError>;

    /// Parses and runs a call in one step, **without consulting policy**.
    ///
    /// A convenience for non-gated callers and tests: exactly
    /// [`prepare`](Self::prepare) then [`dispatch`](Self::dispatch), discarding
    /// the permission request. The gated path the runner uses is `gate.prepare`
    /// → `decide` → [`dispatch`](Self::dispatch); do **not** wire `call` into a
    /// permission-sensitive path.
    async fn call(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        match self.prepare(args, ctx) {
            Ok(prepared) => self.dispatch(prepared, ctx).await,
            Err(rejected) => Ok(rejected),
        }
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
    ///
    /// `'static` so a parsed value can be type-erased into a
    /// [`PreparedToolCall`] and downcast back at [`dispatch`](Tool::dispatch).
    type Args: DeserializeOwned + JsonSchema + Send + 'static;

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

    fn prepare(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<PreparedToolCall, ToolOutput> {
        // Parse once, by reference, so `raw` stays intact for hooks/audit and
        // `dispatch` never re-parses. A parse failure fails here as
        // `invalid_arguments` and never reaches the approver.
        let parsed = match T::Args::deserialize(&args) {
            Ok(parsed) => parsed,
            Err(err) => {
                return Err(ToolOutput::failure(
                    ToolErrorKind::InvalidArguments,
                    format!("failed to parse arguments: {err}"),
                ));
            }
        };
        let request = TypedTool::permission(self, &parsed, ctx);
        Ok(PreparedToolCall::new(request, args, parsed))
    }

    async fn dispatch(
        &self,
        prepared: PreparedToolCall,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        // Downcast back to this tool's own `Args`. Paired correctly by the
        // runner this never fails; a mismatch means a prepared call was routed
        // to the wrong tool — a harness bug, surfaced (not panicked) as Internal.
        let parsed = prepared.parsed.downcast::<T::Args>().map_err(|_| {
            ToolError::Internal(format!(
                "prepared call routed to the wrong tool `{}`",
                TypedTool::definition(self).name
            ))
        })?;
        self.run(*parsed, ctx).await.erase()
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
