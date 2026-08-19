//! The `task` tool: delegate a self-contained subtask to a subagent.
//!
//! A subagent is a nested agent loop. Its shape is picked by name from the
//! [`AgentTypeCatalog`]: the default `general` type starts from a fresh
//! transcript containing only the delegated prompt, the built-in `fork` type
//! starts from a copy of the parent conversation, and custom types can narrow
//! the tool set and append their own instructions. Every shape shares the
//! parent's workspace, permission gates, and approval channel; only the final
//! report returns to the parent, so exploratory tool traffic never enters the
//! parent's context. The tool itself holds no runtime — the runner injects a
//! [`SubagentDriver`] through [`ToolContext::subagents`], keeping this adapter
//! free of the model type and the registry cycle.

use std::sync::Arc;

use async_trait::async_trait;
use kuncode_core::completion::{ToolDefinition, Usage};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    agent_type::{AgentType, AgentTypeCatalog, GENERAL_AGENT_TYPE},
    permission::{CanonicalToolInput, PermissionCheckSpec, PermissionTarget, ToolDisplay},
    tool::{
        PreparationContext, ToolContext, ToolErrorKind, ToolOutput, TypedPreparation, TypedTool,
        definition_for, output::truncate_utf8,
    },
};

/// Model-facing name, also used by the runner to exclude this tool from the
/// registry a subagent receives.
pub const TASK_TOOL_NAME: &str = "task";

/// Reports are the distilled result of a run, so they share the bound used for
/// raw `bash` output rather than getting a larger one.
const REPORT_LIMIT_BYTES: usize = 20_000;

const DESCRIPTION: &str = "\
Delegate a self-contained subtask to a subagent: a nested agent loop that \
shares this workspace and its permission rules. It works with the same tools \
(except task itself) and returns only its final report; the intermediate steps \
never enter your context. Use it for exploratory or multi-step work whose \
intermediate output would flood the conversation, such as searching a large \
codebase or running and digesting a long build.

Pick the subagent's shape with agent_type. The default `general` starts from a \
fresh context holding only your prompt, so make that prompt self-contained: \
state the goal, the constraints, and exactly what the final report must \
contain. `fork` instead starts from a copy of this conversation, for subtasks \
that depend on what was already discussed; it still cannot ask questions, so \
the prompt must say what to do and what to report.";

/// Arguments for [`Task`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskArgs {
    /// Short label (3-7 words) of the subtask, shown to the user while the
    /// subagent runs, e.g. "Find failing test causes".
    pub description: String,
    /// Complete, self-contained instructions for the subagent. Except with
    /// agent_type `fork`, it sees nothing else of this conversation, so
    /// include all context it needs and say what its final report must
    /// contain.
    pub prompt: String,
    /// Which agent type runs the subtask, from the list in this tool's
    /// description. Defaults to `general`.
    pub agent_type: Option<String>,
}

/// The only part of a subagent run that returns to the parent transcript.
#[derive(Debug, Serialize)]
pub struct TaskOutput {
    /// The subagent's final report.
    pub report: String,
    /// Model calls the subagent used to produce the report.
    pub iterations: usize,
}

/// One delegation as handed to the [`SubagentDriver`]: the validated prompt
/// plus the resolved shape to run it under.
#[derive(Clone, Debug)]
pub struct SubagentRequest {
    /// Display/log label; must not influence the run.
    pub description: String,
    /// The delegated instructions.
    pub prompt: String,
    /// Resolved shape — context mode, tool whitelist, extra instructions. The
    /// permission check for `Agent(<name>)` already passed by the time the
    /// driver sees this.
    pub agent_type: AgentType,
}

/// Successful subagent run as seen by the delegating tool.
#[derive(Clone, Debug)]
pub struct SubagentOutcome {
    /// Final assistant text of the subagent's turn.
    pub report: String,
    /// Provider usage the run consumed. Informational here — the driver is
    /// responsible for folding it into the parent turn's accounting.
    pub usage: Usage,
    /// Model calls the run performed.
    pub iterations: usize,
}

/// Model-recoverable subagent failure, pre-classified by the driver.
#[derive(Clone, Debug)]
pub struct SubagentFailure {
    /// Stable failure category surfaced in the tool result.
    pub kind: ToolErrorKind,
    /// Bounded diagnostic for the model to react to.
    pub message: String,
}

impl SubagentFailure {
    /// Builds a classified failure.
    pub fn new(kind: impl Into<ToolErrorKind>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }
}

/// Runs one delegated subagent turn on behalf of the [`Task`] tool.
///
/// Implemented by the runner, which owns the model handle and the registry;
/// the tool layer stays ignorant of both. Implementations must honor `cancel`
/// so a user interrupt unwinds the nested loop, and must account the run's
/// usage toward the parent turn.
#[async_trait]
pub trait SubagentDriver: Send + Sync {
    /// Executes one delegation and returns its report.
    async fn run(
        &self,
        request: SubagentRequest,
        cancel: &CancellationToken,
    ) -> Result<SubagentOutcome, SubagentFailure>;
}

/// Delegates a subtask to a subagent. See the [module docs](self).
#[derive(Clone, Debug)]
pub struct Task {
    definition: ToolDefinition,
    catalog: Arc<AgentTypeCatalog>,
}

impl Task {
    /// Creates the tool over the built-in agent types only.
    pub fn new() -> Self {
        Self::with_types(Arc::new(AgentTypeCatalog::builtin()))
    }

    /// Creates the tool over a scanned catalog. The type list renders into the
    /// tool description — startup-static, like the rest of the definition — so
    /// the model picks types from the schema without a discovery step. The
    /// loop it delegates to arrives per call via [`ToolContext::subagents`].
    pub fn with_types(catalog: Arc<AgentTypeCatalog>) -> Self {
        let mut description = String::from(DESCRIPTION);
        description.push_str("\n\nAgent types:\n");
        for agent_type in catalog.types() {
            description.push_str("- ");
            description.push_str(agent_type.name());
            if !agent_type.description().is_empty() {
                description.push_str(": ");
                description.push_str(agent_type.description());
            }
            description.push('\n');
        }
        Self {
            definition: definition_for::<TaskArgs>(TASK_TOOL_NAME, description.trim_end()),
            catalog,
        }
    }

    /// A bounded name list for the not-found message, so a typo gets the model
    /// back on track without a second discovery step.
    fn known_types(&self) -> String {
        self.catalog
            .types()
            .map(AgentType::name)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl Default for Task {
    fn default() -> Self {
        Self::new()
    }
}

/// Payload retained between preparation and execution.
pub struct PreparedTask {
    description: String,
    prompt: String,
    agent_type: AgentType,
}

#[async_trait]
impl TypedTool for Task {
    type Args = TaskArgs;
    type Prepared = PreparedTask;
    type Output = TaskOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        args: TaskArgs,
        canonical_input: CanonicalToolInput,
        _ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        // A blank prompt would start a subagent with nothing to do; refuse
        // before anyone is asked to authorize it.
        if args.prompt.trim().is_empty() {
            return Err(ToolOutput::failure(
                ToolErrorKind::InvalidArguments,
                "task prompt must not be blank",
            ));
        }
        if args.description.trim().is_empty() {
            return Err(ToolOutput::failure(
                ToolErrorKind::InvalidArguments,
                "task description must not be blank",
            ));
        }
        let requested = args
            .agent_type
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(GENERAL_AGENT_TYPE);
        let Some(agent_type) = self.catalog.resolve(requested) else {
            return Err(ToolOutput::failure(
                ToolErrorKind::InvalidArguments,
                format!(
                    "unknown agent type `{requested}`; available types: {}",
                    self.known_types()
                ),
            ));
        };
        // The check names the resolved type, so `Agent(<name>)` rules gate
        // each shape independently and Plan mode still denies them all.
        let target = PermissionTarget::agent(agent_type.name()).map_err(|error| {
            ToolOutput::failure(ToolErrorKind::InvalidArguments, error.to_string())
        })?;
        let display = if agent_type.name() == GENERAL_AGENT_TYPE {
            ToolDisplay::new(format!("Task: {}", args.description))
        } else {
            ToolDisplay::new(format!(
                "Task ({}): {}",
                agent_type.name(),
                args.description
            ))
        };
        Ok(TypedPreparation::new(
            PreparedTask {
                description: args.description,
                prompt: args.prompt,
                agent_type: agent_type.clone(),
            },
            canonical_input,
            kuncode_core::non_empty_vec::NonEmptyVec::new(PermissionCheckSpec::new(target)),
            display,
        ))
    }

    async fn run_prepared(
        &self,
        prepared: PreparedTask,
        ctx: &ToolContext,
    ) -> ToolOutput<TaskOutput> {
        let Some(driver) = &ctx.subagents else {
            // Reachable outside a runner turn (tests, direct embedders): a
            // model-recoverable failure, not a harness error, so the loop
            // continues without this delegation.
            return ToolOutput::failure(
                "subagent_unavailable",
                "no subagent runtime is attached to this session",
            );
        };
        let request = SubagentRequest {
            description: prepared.description,
            prompt: prepared.prompt,
            agent_type: prepared.agent_type,
        };
        match driver.run(request, &ctx.cancel).await {
            Ok(outcome) => {
                let (report, truncated) = truncate_utf8(&outcome.report, REPORT_LIMIT_BYTES);
                let output = ToolOutput::success(TaskOutput {
                    report,
                    iterations: outcome.iterations,
                });
                if truncated {
                    output.truncated()
                } else {
                    output
                }
            }
            Err(failure) => ToolOutput::failure(failure.kind, failure.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::agent_type::FORK_AGENT_TYPE;
    use crate::tool::{Tool, execute_for_test};

    /// Driver stub scripted with one fixed result; records the request.
    struct ScriptedDriver {
        result: Result<SubagentOutcome, SubagentFailure>,
        seen: std::sync::Mutex<Vec<SubagentRequest>>,
    }

    impl ScriptedDriver {
        fn new(result: Result<SubagentOutcome, SubagentFailure>) -> Self {
            Self {
                result,
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl SubagentDriver for ScriptedDriver {
        async fn run(
            &self,
            request: SubagentRequest,
            _cancel: &CancellationToken,
        ) -> Result<SubagentOutcome, SubagentFailure> {
            self.seen.lock().expect("request log").push(request);
            self.result.clone()
        }
    }

    fn ctx_with(driver: Arc<ScriptedDriver>) -> ToolContext {
        ToolContext::new().with_subagents(driver)
    }

    fn args() -> serde_json::Value {
        serde_json::json!({
            "description": "Inspect workspace",
            "prompt": "Count the crates and report their names."
        })
    }

    fn outcome(report: &str) -> SubagentOutcome {
        SubagentOutcome {
            report: report.to_string(),
            usage: Usage::default(),
            iterations: 3,
        }
    }

    /// A catalog holding the built-ins plus one custom `explore` type.
    fn catalog_with_explore() -> Arc<AgentTypeCatalog> {
        let root = std::env::temp_dir().join(format!(
            "kuncode-task-types-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("t")
                .replace(':', "_"),
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp dir");
        std::fs::write(
            root.join("explore.md"),
            "---\ndescription: Read-only exploration.\ntools: read_file, grep\n---\nOnly report findings.",
        )
        .expect("definition");
        let catalog = AgentTypeCatalog::scan(std::slice::from_ref::<PathBuf>(&root));
        let _ = std::fs::remove_dir_all(&root);
        Arc::new(catalog)
    }

    #[tokio::test]
    async fn returns_only_the_subagent_report() {
        let driver = Arc::new(ScriptedDriver::new(Ok(outcome("42 crates"))));
        let ctx = ctx_with(driver.clone());

        let output = execute_for_test(Arc::new(Task::new()), args(), &ctx)
            .await
            .expect("no harness error");

        assert!(output.ok);
        let data = output.data.expect("data present");
        assert_eq!(data["report"], "42 crates");
        assert_eq!(data["iterations"], 3);
        // No agent_type argument resolves to the built-in default.
        let seen = driver.seen.lock().expect("request log");
        assert_eq!(seen[0].agent_type.name(), GENERAL_AGENT_TYPE);
    }

    #[tokio::test]
    async fn oversized_reports_are_bounded_and_marked_truncated() {
        let ctx = ctx_with(Arc::new(ScriptedDriver::new(Ok(outcome(
            &"x".repeat(30_000),
        )))));

        let output = execute_for_test(Arc::new(Task::new()), args(), &ctx)
            .await
            .expect("no harness error");

        assert!(output.ok);
        assert!(output.truncated);
        let report = output.data.expect("data present")["report"]
            .as_str()
            .expect("report is text")
            .to_string();
        assert_eq!(report.len(), REPORT_LIMIT_BYTES);
    }

    #[tokio::test]
    async fn driver_failures_pass_through_with_their_kind() {
        let ctx = ctx_with(Arc::new(ScriptedDriver::new(Err(SubagentFailure::new(
            ToolErrorKind::Cancelled,
            "subagent turn was cancelled",
        )))));

        let output = execute_for_test(Arc::new(Task::new()), args(), &ctx)
            .await
            .expect("no harness error");

        assert!(!output.ok);
        let error = output.error.expect("error present");
        assert_eq!(error.kind, ToolErrorKind::Cancelled);
    }

    #[tokio::test]
    async fn missing_driver_is_a_recoverable_failure() {
        let output = execute_for_test(Arc::new(Task::new()), args(), &ToolContext::new())
            .await
            .expect("no harness error");

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "subagent_unavailable"
        );
    }

    #[tokio::test]
    async fn blank_prompt_is_rejected_before_preparation() {
        let blank = serde_json::json!({ "description": "d", "prompt": "  " });

        let output = execute_for_test(Arc::new(Task::new()), blank, &ToolContext::new())
            .await
            .expect("no harness error");

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind,
            ToolErrorKind::InvalidArguments
        );
    }

    #[tokio::test]
    async fn unknown_agent_types_fail_and_list_the_catalog() {
        let unknown = serde_json::json!({
            "description": "d",
            "prompt": "p",
            "agent_type": "explorer"
        });

        let output = execute_for_test(Arc::new(Task::new()), unknown, &ToolContext::new())
            .await
            .expect("no harness error");

        assert!(!output.ok);
        let error = output.error.expect("error present");
        assert_eq!(error.kind, ToolErrorKind::InvalidArguments);
        assert!(error.message.contains("general"), "{}", error.message);
        assert!(error.message.contains("fork"), "{}", error.message);
    }

    #[tokio::test]
    async fn preparation_emits_the_agent_namespace_with_the_resolved_type() {
        let preparation = Arc::new(Task::new())
            .prepare(args(), &PreparationContext::new())
            .await
            .expect("valid preparation");

        assert!(matches!(
            preparation.checks().first().target(),
            PermissionTarget::Agent(profile) if profile == GENERAL_AGENT_TYPE
        ));
        assert_eq!(preparation.display().summary(), "Task: Inspect workspace");
    }

    #[tokio::test]
    async fn a_named_type_reaches_the_check_the_display_and_the_driver() {
        let fork_args = serde_json::json!({
            "description": "Summarize decisions",
            "prompt": "Summarize what we decided so far.",
            "agent_type": "fork"
        });
        let tool = Arc::new(Task::new());

        let preparation = tool
            .clone()
            .prepare(fork_args.clone(), &PreparationContext::new())
            .await
            .expect("valid preparation");
        assert!(matches!(
            preparation.checks().first().target(),
            PermissionTarget::Agent(profile) if profile == FORK_AGENT_TYPE
        ));
        assert_eq!(
            preparation.display().summary(),
            "Task (fork): Summarize decisions"
        );

        let driver = Arc::new(ScriptedDriver::new(Ok(outcome("summary"))));
        let output = execute_for_test(tool, fork_args, &ctx_with(driver.clone()))
            .await
            .expect("no harness error");
        assert!(output.ok);
        let seen = driver.seen.lock().expect("request log");
        assert_eq!(seen[0].agent_type.name(), FORK_AGENT_TYPE);
        assert_eq!(
            seen[0].agent_type.context(),
            crate::agent_type::SubagentContext::Fork
        );
    }

    #[tokio::test]
    async fn custom_types_are_advertised_and_resolved() {
        let tool = Task::with_types(catalog_with_explore());

        let description = &tool.definition.description;
        assert!(
            description.contains("- explore: Read-only exploration."),
            "{description}"
        );

        let explore_args = serde_json::json!({
            "description": "Map the crate layout",
            "prompt": "List every crate and its purpose.",
            "agent_type": "explore"
        });
        let driver = Arc::new(ScriptedDriver::new(Ok(outcome("mapped"))));
        let output = execute_for_test(Arc::new(tool), explore_args, &ctx_with(driver.clone()))
            .await
            .expect("no harness error");

        assert!(output.ok);
        let seen = driver.seen.lock().expect("request log");
        assert_eq!(seen[0].agent_type.name(), "explore");
        assert_eq!(
            seen[0].agent_type.tools().expect("whitelist present"),
            ["read_file", "grep"]
        );
        assert_eq!(
            seen[0].agent_type.instructions(),
            Some("Only report findings.")
        );
    }
}
