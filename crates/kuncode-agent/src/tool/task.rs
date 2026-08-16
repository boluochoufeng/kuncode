//! The `task` tool: delegate a self-contained subtask to a subagent.
//!
//! A subagent is a nested agent loop that starts from a fresh transcript
//! containing only the delegated prompt. It shares the parent's workspace,
//! permission gates, and approval channel, but none of its conversation: only
//! the final report returns to the parent, so exploratory tool traffic never
//! enters the parent's context. The tool itself holds no runtime — the runner
//! injects a [`SubagentDriver`] through [`ToolContext::subagents`], keeping
//! this adapter free of the model type and the registry cycle.

use async_trait::async_trait;
use kuncode_core::completion::{ToolDefinition, Usage};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    permission::{CanonicalToolInput, PermissionCheckSpec, PermissionTarget, ToolDisplay},
    tool::{
        PreparationContext, ToolContext, ToolErrorKind, ToolOutput, TypedPreparation, TypedTool,
        definition_for, output::truncate_utf8,
    },
};

/// Model-facing name, also used by the runner to exclude this tool from the
/// registry a subagent receives.
pub const TASK_TOOL_NAME: &str = "task";

/// Permission profile name carried by the `Agent(...)` check. There is a single
/// built-in subagent shape today; a named profile keeps rules like
/// `Agent(general)` stable when specialized profiles arrive.
pub const SUBAGENT_PROFILE: &str = "general";

/// Reports are the distilled result of a run, so they share the bound used for
/// raw `bash` output rather than getting a larger one.
const REPORT_LIMIT_BYTES: usize = 20_000;

const DESCRIPTION: &str = "\
Delegate a self-contained subtask to a subagent: a fresh agent loop that shares \
this workspace and its permission rules but none of this conversation. It works \
with the same tools (except task itself) and returns only its final report; the \
intermediate steps never enter your context. Use it for exploratory or \
multi-step work whose intermediate output would flood the conversation, such as \
searching a large codebase or running and digesting a long build.

The subagent starts from your prompt alone and cannot ask questions, so make \
the prompt self-contained: state the goal, the constraints, and exactly what \
the final report must contain.";

/// Arguments for [`Task`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskArgs {
    /// Short label (3-7 words) of the subtask, shown to the user while the
    /// subagent runs, e.g. "Find failing test causes".
    pub description: String,
    /// Complete, self-contained instructions for the subagent. It sees nothing
    /// else of this conversation, so include all context it needs and say what
    /// its final report must contain.
    pub prompt: String,
}

/// The only part of a subagent run that returns to the parent transcript.
#[derive(Debug, Serialize)]
pub struct TaskOutput {
    /// The subagent's final report.
    pub report: String,
    /// Model calls the subagent used to produce the report.
    pub iterations: usize,
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
    /// Executes `prompt` in a fresh subagent session and returns its report.
    ///
    /// `description` is display/log context only and must not influence the
    /// run.
    async fn run(
        &self,
        description: &str,
        prompt: &str,
        cancel: &CancellationToken,
    ) -> Result<SubagentOutcome, SubagentFailure>;
}

/// Delegates a subtask to a subagent. See the [module docs](self).
#[derive(Clone, Debug)]
pub struct Task {
    definition: ToolDefinition,
}

impl Task {
    /// Creates the tool. Holds only its cached definition; the loop it
    /// delegates to arrives per call via [`ToolContext::subagents`].
    pub fn new() -> Self {
        Self {
            definition: definition_for::<TaskArgs>(TASK_TOOL_NAME, DESCRIPTION),
        }
    }
}

impl Default for Task {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TypedTool for Task {
    type Args = TaskArgs;
    type Prepared = TaskArgs;
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
        let target = PermissionTarget::agent(SUBAGENT_PROFILE).map_err(|error| {
            ToolOutput::failure(ToolErrorKind::InvalidArguments, error.to_string())
        })?;
        let display = ToolDisplay::new(format!("Task: {}", args.description));
        Ok(TypedPreparation::new(
            args,
            canonical_input,
            kuncode_core::non_empty_vec::NonEmptyVec::new(PermissionCheckSpec::new(target)),
            display,
        ))
    }

    async fn run_prepared(&self, prepared: TaskArgs, ctx: &ToolContext) -> ToolOutput<TaskOutput> {
        let Some(driver) = &ctx.subagents else {
            // Reachable outside a runner turn (tests, direct embedders): a
            // model-recoverable failure, not a harness error, so the loop
            // continues without this delegation.
            return ToolOutput::failure(
                "subagent_unavailable",
                "no subagent runtime is attached to this session",
            );
        };
        match driver
            .run(&prepared.description, &prepared.prompt, &ctx.cancel)
            .await
        {
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
    use std::sync::Arc;

    use super::*;
    use crate::tool::{Tool, execute_for_test};

    /// Driver stub scripted with one fixed result.
    struct ScriptedDriver(Result<SubagentOutcome, SubagentFailure>);

    #[async_trait]
    impl SubagentDriver for ScriptedDriver {
        async fn run(
            &self,
            _description: &str,
            _prompt: &str,
            _cancel: &CancellationToken,
        ) -> Result<SubagentOutcome, SubagentFailure> {
            self.0.clone()
        }
    }

    fn ctx_with(driver: ScriptedDriver) -> ToolContext {
        ToolContext::new().with_subagents(Arc::new(driver))
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

    #[tokio::test]
    async fn returns_only_the_subagent_report() {
        let ctx = ctx_with(ScriptedDriver(Ok(outcome("42 crates"))));

        let output = execute_for_test(Arc::new(Task::new()), args(), &ctx)
            .await
            .expect("no harness error");

        assert!(output.ok);
        let data = output.data.expect("data present");
        assert_eq!(data["report"], "42 crates");
        assert_eq!(data["iterations"], 3);
    }

    #[tokio::test]
    async fn oversized_reports_are_bounded_and_marked_truncated() {
        let ctx = ctx_with(ScriptedDriver(Ok(outcome(&"x".repeat(30_000)))));

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
        let ctx = ctx_with(ScriptedDriver(Err(SubagentFailure::new(
            ToolErrorKind::Cancelled,
            "subagent turn was cancelled",
        ))));

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
    async fn preparation_emits_the_agent_namespace() {
        let preparation = Arc::new(Task::new())
            .prepare(args(), &PreparationContext::new())
            .await
            .expect("valid preparation");

        assert!(matches!(
            preparation.checks().first().target(),
            PermissionTarget::Agent(profile) if profile == SUBAGENT_PROFILE
        ));
        assert_eq!(preparation.display().summary(), "Task: Inspect workspace");
    }
}
