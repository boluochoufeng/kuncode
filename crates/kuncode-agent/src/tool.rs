//! Tool interface exposed by the agent runtime.

use std::sync::Arc;

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use tokio_util::sync::CancellationToken;

use crate::permission::{
    CanonicalToolInput, PathVisibility, PermissionCheckSpec, PermissionTarget, ToolDisplay,
};
use crate::todo::TodoHandle;

pub mod bash;
pub mod filesystem;
pub mod todo_write;
pub mod web_fetch;

mod output;
mod read_ledger;

pub use output::{ToolError, ToolErrorKind, ToolErrorPayload, ToolOutput, ToolResultRetention};
pub use read_ledger::{FileStamp, ReadLedger, ReadState};

/// Stable context available while preparing a call, before it is authorized.
///
/// Preparation must stay side-effect-free — it runs before any check has
/// passed, and its result is what hooks inspect and fingerprints cover — so
/// what belongs here is read-only session knowledge a tool needs in order to
/// describe or refuse a call, never a capability to act on one.
///
/// A refusal a tool can already reach at this point belongs at this point: the
/// alternative is asking the user to approve a call that was never going to
/// run.
#[derive(Clone, Debug, Default)]
pub struct PreparationContext {
    /// Files this session has read, so a tool that would destroy contents
    /// nobody has seen can say so before the approval prompt rather than after
    /// it. The same ledger reaches execution through
    /// [`ToolContext::reads`], where the check runs again against the file as
    /// it stands once approval is done.
    pub reads: ReadLedger,
}

impl PreparationContext {
    /// Creates a preparation context with a standalone ledger, which is what
    /// tests and non-interactive callers want.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attaches the session's reading history.
    pub fn with_reads(mut self, reads: ReadLedger) -> Self {
        self.reads = reads;
        self
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
    /// Read paths a walking tool must not surface, compiled from the effective
    /// policy by [`AuthorizationEngine::execute`] rather than by the tool.
    ///
    /// A tool consults it; nothing here lets a tool widen it. The default hides
    /// nothing, which is why the engine attaches the real one itself instead of
    /// trusting its caller to pass a context that has it.
    ///
    /// [`AuthorizationEngine::execute`]: crate::permission::AuthorizationEngine::execute
    pub visibility: PathVisibility,
    /// Files this session has read, consulted by `write_file` before it
    /// truncates one. Like [`todos`](Self::todos), the runner clones in the
    /// session's handle; the default is standalone, so a tool that ignores it
    /// still gets a valid target.
    pub reads: ReadLedger,
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

    /// Attaches the session's reading history, so `write_file` sees what
    /// earlier calls in the same session read rather than a fresh empty ledger.
    pub fn with_reads(mut self, reads: ReadLedger) -> Self {
        self.reads = reads;
        self
    }

    pub(crate) fn with_visibility(mut self, visibility: PathVisibility) -> Self {
        self.visibility = visibility;
        self
    }

    /// The part of this context a tool may consult before its call is
    /// authorized.
    ///
    /// Derived here rather than assembled by the caller so the two contexts
    /// cannot drift: whatever the session hands to execution is what
    /// preparation sees a read-only view of.
    pub fn preparation(&self) -> PreparationContext {
        PreparationContext::new().with_reads(self.reads.clone())
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
    let preparation = match tool.prepare(args, &ctx.preparation()).await {
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
///
/// Argument doc comments serve two readers at once: `cargo doc` renders them
/// for a Rust developer, and the model receives the same text as its only
/// instructions. Everything below reconciles the two, so a doc comment can stay
/// idiomatic Rust without shipping rustdoc artifacts to the model.
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
        // The argument struct's own doc ("Arguments accepted by ...") restates
        // what the caller already knows from the tool it invoked. The tool's
        // `description` is what carries this role.
        obj.remove("description");
    }
    strip_doc_link_markup(&mut parameters);

    ToolDefinition {
        name: name.into(),
        description: description.into(),
        parameters,
    }
}

/// Rewrites rustdoc intra-doc links in every `description` the schema carries.
fn strip_doc_link_markup(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                match child.as_str() {
                    Some(text) if key == "description" => {
                        *child = serde_json::Value::String(strip_doc_links(text));
                    }
                    _ => strip_doc_link_markup(child),
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_doc_link_markup),
        _ => {}
    }
}

/// Removes rustdoc link markup, keeping the link text.
///
/// `` [`NonEmptyVec`] `` becomes `` `NonEmptyVec` `` and `[text](Self::field)`
/// becomes `text`: rustdoc renders these as hyperlinks, but the model sees the
/// raw brackets and a Rust path that names nothing it can reach.
///
/// Two things are deliberately left alone. A link to a real URL renders the
/// same for both readers, so it stays whole. And a bare `[...]` is only
/// unwrapped when the text inside is code-quoted — otherwise ordinary prose
/// like "an array [1, 2]" would lose its brackets.
fn strip_doc_links(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;

    // Bracket and parenthesis are ASCII, and no byte of a multi-byte UTF-8
    // sequence is, so scanning by byte cannot split a character.
    while let Some(offset) = bytes[cursor..].iter().position(|&byte| byte == b'[') {
        let open = cursor + offset;
        let Some(offset) = bytes[open + 1..].iter().position(|&byte| byte == b']') else {
            break;
        };
        let close = open + 1 + offset;
        let label = &text[open + 1..close];
        out.push_str(&text[cursor..open]);

        let after = close + 1;
        let target_end = if bytes.get(after) == Some(&b'(') {
            bytes[after + 1..]
                .iter()
                .position(|&byte| byte == b')')
                .map(|offset| after + 1 + offset)
        } else {
            None
        };

        match target_end {
            Some(end) if text[after + 1..end].starts_with("http") => {
                out.push_str(&text[open..=end]);
                cursor = end + 1;
            }
            Some(end) => {
                out.push_str(label);
                cursor = end + 1;
            }
            None => {
                if is_code_quoted(label) {
                    out.push_str(label);
                } else {
                    out.push_str(&text[open..=close]);
                }
                cursor = close + 1;
            }
        }
    }

    out.push_str(&text[cursor..]);
    out
}

fn is_code_quoted(label: &str) -> bool {
    label.len() > 2 && label.starts_with('`') && label.ends_with('`')
}

#[cfg(test)]
mod definition_tests {
    use schemars::JsonSchema;

    use super::{definition_for, strip_doc_links};

    #[test]
    fn rustdoc_links_lose_their_markup_but_keep_their_text() {
        assert_eq!(
            strip_doc_links("Arguments accepted by the [`Bash`] tool."),
            "Arguments accepted by the `Bash` tool."
        );
        assert_eq!(
            strip_doc_links("shown while [`InProgress`](TodoStatus::InProgress)"),
            "shown while `InProgress`"
        );
        assert_eq!(
            strip_doc_links("see [the docs](Self::build)"),
            "see the docs"
        );
    }

    #[test]
    fn prose_and_real_links_are_left_alone() {
        // A URL renders as a link for both readers, so there is nothing to fix.
        assert_eq!(
            strip_doc_links("see [the spec](https://example.com/spec)"),
            "see [the spec](https://example.com/spec)"
        );
        // Brackets around ordinary prose are not link markup.
        assert_eq!(
            strip_doc_links("an array [1, 2] here"),
            "an array [1, 2] here"
        );
        // An unclosed bracket is text, not a truncated link.
        assert_eq!(strip_doc_links("a [ b"), "a [ b");
    }

    /// Doc of the arguments struct, which the model has no use for.
    #[derive(JsonSchema)]
    #[allow(dead_code)]
    struct Args {
        /// Pass it to [`Args::other`], see [`crate::tool::Tool`].
        field: String,
        /// Second one.
        other: u8,
    }

    #[test]
    fn generated_schema_drops_struct_doc_and_link_markup() {
        let definition = definition_for::<Args>("t", "a tool");
        let parameters = &definition.parameters;

        // The struct's own doc restates the tool the caller already chose.
        assert!(parameters.get("description").is_none());
        assert_eq!(
            parameters["properties"]["field"]["description"],
            serde_json::json!("Pass it to `Args::other`, see `crate::tool::Tool`.")
        );
    }
}
