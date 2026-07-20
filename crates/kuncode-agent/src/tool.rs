//! Tool interface exposed by the agent runtime.

use std::sync::Arc;

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use tokio_util::sync::CancellationToken;

use crate::permission::{CanonicalToolInput, PermissionCheckSpec, PermissionTarget, ToolDisplay};
use crate::todo::TodoHandle;

pub mod bash;
pub mod filesystem;
pub mod todo_write;

mod output;

pub use output::{ToolError, ToolErrorKind, ToolErrorPayload, ToolOutput, ToolResultRetention};

/// Stable, capability-free context available while preparing a call.
#[derive(Clone, Debug, Default)]
pub struct PreparationContext;

impl PreparationContext {
    /// Creates an empty preparation context.
    pub const fn new() -> Self {
        Self
    }
}

/// Output plus harness-owned retention selected by the executed invocation.
pub struct ExecutedInvocation {
    output: ToolOutput,
    retention: ToolResultRetention,
}

/// Result of checking whether a retained payload still names the same resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparedInvocationState {
    /// Security-relevant metadata still matches preparation.
    Current,
    /// The caller must discard the receipt and prepare the canonical input again.
    Stale,
}

impl ExecutedInvocation {
    /// Binds one delivered output to its authoritative retention decision.
    pub fn new(output: ToolOutput, retention: ToolResultRetention) -> Self {
        Self { output, retention }
    }

    /// Splits the delivered output from its retention metadata.
    pub fn into_parts(self) -> (ToolOutput, ToolResultRetention) {
        (self.output, self.retention)
    }
}

/// Parsed executable payload retained across authorization without raw reparse.
#[async_trait]
pub trait PreparedInvocation: Send {
    /// Rechecks metadata that may change while approval is pending.
    async fn revalidate(
        &mut self,
        _ctx: &ToolContext,
    ) -> Result<PreparedInvocationState, ToolError> {
        Ok(PreparedInvocationState::Current)
    }

    /// Consumes the payload exactly once.
    async fn execute(self: Box<Self>, ctx: &ToolContext) -> Result<ExecutedInvocation, ToolError>;
}

/// Side-effect-free preparation returned before registry profile validation.
pub struct ToolPreparation {
    canonical_input: CanonicalToolInput,
    invocation: Box<dyn PreparedInvocation>,
    checks: NonEmptyVec<PermissionCheckSpec>,
    display: ToolDisplay,
}

impl ToolPreparation {
    /// Creates a complete preparation with at least one permission check.
    pub fn new(
        canonical_input: CanonicalToolInput,
        invocation: Box<dyn PreparedInvocation>,
        checks: NonEmptyVec<PermissionCheckSpec>,
        display: ToolDisplay,
    ) -> Self {
        Self {
            canonical_input,
            invocation,
            checks,
            display,
        }
    }

    /// Returns the normalized JSON exposed to authorization hooks.
    pub fn canonical_input(&self) -> &CanonicalToolInput {
        &self.canonical_input
    }

    /// Returns the unvalidated checks emitted by the tool adapter.
    pub fn checks(&self) -> &NonEmptyVec<PermissionCheckSpec> {
        &self.checks
    }

    /// Returns safe display text that never participates in authorization.
    pub fn display(&self) -> &ToolDisplay {
        &self.display
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        CanonicalToolInput,
        Box<dyn PreparedInvocation>,
        NonEmptyVec<PermissionCheckSpec>,
        ToolDisplay,
    ) {
        (
            self.canonical_input,
            self.invocation,
            self.checks,
            self.display,
        )
    }
}

/// Typed preparation metadata paired with a tool-specific parsed payload.
pub struct TypedPreparation<P> {
    prepared: P,
    canonical_input: CanonicalToolInput,
    checks: NonEmptyVec<PermissionCheckSpec>,
    display: ToolDisplay,
}

impl<P> TypedPreparation<P> {
    /// Builds metadata and the payload that execution will consume.
    pub fn new(
        prepared: P,
        canonical_input: CanonicalToolInput,
        checks: NonEmptyVec<PermissionCheckSpec>,
        display: ToolDisplay,
    ) -> Self {
        Self {
            prepared,
            canonical_input,
            checks,
            display,
        }
    }
}

/// Builds the conservative exact-tool preparation used while an adapter has no
/// narrower trusted namespace implementation.
///
/// # Errors
/// Returns an invalid-arguments output when the registered tool name is blank.
pub fn exact_typed_preparation<P>(
    tool: &str,
    prepared: P,
    canonical_input: CanonicalToolInput,
    display: ToolDisplay,
) -> Result<TypedPreparation<P>, ToolOutput> {
    let target = PermissionTarget::exact_tool(tool)
        .map_err(|error| ToolOutput::failure(ToolErrorKind::InvalidArguments, error.to_string()))?;
    Ok(TypedPreparation::new(
        prepared,
        canonical_input,
        NonEmptyVec::new(PermissionCheckSpec::new(target)),
        display,
    ))
}

/// Per-call execution context threaded into every tool.
///
/// Kept deliberately small. Authorization lives in the runner, not here, and tools already self-hold
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
pub trait Tool: Send + Sync + 'static {
    /// Definition (name, description, argument schema) advertised to the model.
    fn definition(&self) -> &ToolDefinition;

    /// Tool name as the model sees it. Defaults to the definition's name so
    /// there is a single source of truth.
    fn name(&self) -> &str {
        &self.definition().name
    }

    /// Parses and canonicalizes a call without performing its business action.
    ///
    /// The returned invocation is the only payload the authorization engine may
    /// execute; implementations must not defer raw-input parsing until execution.
    async fn prepare(
        self: Arc<Self>,
        args: serde_json::Value,
        ctx: &PreparationContext,
    ) -> Result<ToolPreparation, ToolOutput>;
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

    /// Parsed payload retained between preparation and execution.
    type Prepared: Send;

    /// Serializable `data` payload returned to the model — and available to
    /// Rust callers (sub-agents, todo, …) without re-parsing JSON.
    type Output: Serialize + Send;

    /// Cached definition. Build it once (e.g. in the constructor) with
    /// [`definition_for`] so schema generation isn't repeated per call.
    fn definition(&self) -> &ToolDefinition;

    /// Produces the canonical input, checks, and executable parsed payload.
    async fn prepare_typed(
        &self,
        args: Self::Args,
        canonical_input: CanonicalToolInput,
        ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput>;

    /// Executes the payload retained by [`Self::prepare_typed`].
    async fn run_prepared(
        &self,
        prepared: Self::Prepared,
        ctx: &ToolContext,
    ) -> ToolOutput<Self::Output>;

    /// Rechecks mutable metadata without executing the business operation.
    async fn revalidate_prepared(
        &self,
        _prepared: &mut Self::Prepared,
        _ctx: &ToolContext,
    ) -> Result<PreparedInvocationState, ToolError> {
        Ok(PreparedInvocationState::Current)
    }

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
    T: TypedTool + 'static,
{
    fn definition(&self) -> &ToolDefinition {
        TypedTool::definition(self)
    }

    async fn prepare(
        self: Arc<Self>,
        args: serde_json::Value,
        ctx: &PreparationContext,
    ) -> Result<ToolPreparation, ToolOutput> {
        let parsed = serde_json::from_value::<T::Args>(args.clone()).map_err(|err| {
            ToolOutput::failure(
                ToolErrorKind::InvalidArguments,
                format!("failed to parse arguments: {err}"),
            )
        })?;
        let typed = self
            .prepare_typed(parsed, CanonicalToolInput::new(args), ctx)
            .await?;
        let TypedPreparation {
            prepared,
            canonical_input,
            checks,
            display,
        } = typed;
        let invocation = TypedPreparedInvocation {
            tool: self,
            prepared,
            canonical_input: canonical_input.clone(),
        };
        Ok(ToolPreparation::new(
            canonical_input,
            Box::new(invocation),
            checks,
            display,
        ))
    }
}

struct TypedPreparedInvocation<T>
where
    T: TypedTool,
{
    tool: Arc<T>,
    prepared: T::Prepared,
    canonical_input: CanonicalToolInput,
}

#[async_trait]
impl<T> PreparedInvocation for TypedPreparedInvocation<T>
where
    T: TypedTool + 'static,
{
    async fn revalidate(
        &mut self,
        ctx: &ToolContext,
    ) -> Result<PreparedInvocationState, ToolError> {
        self.tool.revalidate_prepared(&mut self.prepared, ctx).await
    }

    async fn execute(self: Box<Self>, ctx: &ToolContext) -> Result<ExecutedInvocation, ToolError> {
        let Self {
            tool,
            prepared,
            canonical_input,
        } = *self;
        let output = tool.run_prepared(prepared, ctx).await.erase()?;
        let retention = tool.result_retention(canonical_input.as_value(), &output);
        Ok(ExecutedInvocation::new(output, retention))
    }
}

#[cfg(test)]
pub(crate) async fn execute_for_test<T: Tool>(
    tool: Arc<T>,
    args: serde_json::Value,
    ctx: &ToolContext,
) -> Result<ToolOutput, ToolError> {
    let preparation = match tool.prepare(args, &PreparationContext::new()).await {
        Ok(preparation) => preparation,
        Err(output) => return Ok(output),
    };
    let (_, mut invocation, _, _) = preparation.into_parts();
    if invocation.revalidate(ctx).await? == PreparedInvocationState::Stale {
        return Ok(ToolOutput::failure(
            "stale_preparation",
            "prepared tool input changed before execution",
        ));
    }
    let executed = invocation.execute(ctx).await?;
    Ok(executed.into_parts().0)
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
