//! Task-store tools: the model's interface to the durable dependency graph.
//!
//! Six tools over one [`TaskStore`]: four mutators (`create_task`,
//! `update_task`, `claim_task`, `complete_task`) sharing the `TaskWrite`
//! permission namespace — allowed by default like the session plan, but
//! denied in Plan mode because the store is a cross-session disk side effect
//! — and two readers (`get_task`, `list_tasks`) that are ordinary `Read`s,
//! Plan-safe and deniable per path. They live in one file because they share
//! the store, the id grammar, and one failure vocabulary.

use std::path::PathBuf;

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    frontmatter::flatten,
    permission::{
        CanonicalPath, CanonicalToolInput, PermissionCheckSpec, PermissionTarget, ToolDisplay,
    },
    tasks::{
        Task, TaskStatus, TaskStore, TaskStoreError, TaskSummary, deps_satisfied, generate_task_id,
        known_task_ids, validate_task_id, would_cycle,
    },
    tool::{
        PreparationContext, ToolContext, ToolErrorKind, ToolOutput, ToolResultRetention,
        TypedPreparation, TypedTool, definition_for,
    },
};

/// Model-facing tool names.
pub const CREATE_TASK_TOOL_NAME: &str = "create_task";
pub const UPDATE_TASK_TOOL_NAME: &str = "update_task";
pub const CLAIM_TASK_TOOL_NAME: &str = "claim_task";
pub const COMPLETE_TASK_TOOL_NAME: &str = "complete_task";
pub const GET_TASK_TOOL_NAME: &str = "get_task";
pub const LIST_TASKS_TOOL_NAME: &str = "list_tasks";

/// Fixed single-agent identity stamped on claims. Real per-agent identity
/// arrives with the multi-agent coordination work this store prepares for.
const TASK_OWNER: &str = "agent";

/// Character budget for a subject shown in an approval/display line.
const SUBJECT_DISPLAY_CHARS: usize = 80;

/// One shared mapping from store failures onto model-facing error kinds.
fn store_failure<D>(error: TaskStoreError) -> ToolOutput<D> {
    let kind = match &error {
        TaskStoreError::InvalidId => "invalid_arguments",
        TaskStoreError::NotFound(_) => "task_not_found",
        TaskStoreError::Corrupt { .. } => "task_store_corrupt",
        TaskStoreError::IdCollision(_) | TaskStoreError::Io { .. } => "tool_error",
    };
    ToolOutput::failure(kind, error.to_string())
}

/// Validates and trims a model-supplied task id at preparation time.
fn prepare_task_id(raw: &str) -> Result<String, ToolOutput> {
    validate_task_id(raw.trim())
        .map_err(|error| ToolOutput::failure(ToolErrorKind::InvalidArguments, error.to_string()))
}

/// The `TaskWrite` check every mutator emits.
fn task_write_checks() -> NonEmptyVec<PermissionCheckSpec> {
    NonEmptyVec::new(PermissionCheckSpec::new(PermissionTarget::TaskWrite))
}

// ---------------------------------------------------------------------------
// create_task

/// Arguments for [`CreateTask`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateTaskArgs {
    /// One-line name of the work item.
    pub subject: String,
    /// Free-form details future sessions need to pick the task up.
    #[serde(default)]
    pub description: String,
}

/// Result of creating a task.
#[derive(Debug, Serialize)]
pub struct CreateTaskOutput {
    /// The stored task, including its generated id.
    pub task: Task,
}

/// Payload retained between preparation and execution.
pub struct PreparedCreateTask {
    subject: String,
    description: String,
}

/// Creates one task with a runtime-generated id. See the [module docs](self).
pub struct CreateTask {
    definition: ToolDefinition,
    store: TaskStore,
}

impl CreateTask {
    /// Creates the tool over the per-project tasks root.
    pub fn new(root: PathBuf) -> Self {
        Self {
            definition: definition_for::<CreateTaskArgs>(
                CREATE_TASK_TOOL_NAME,
                "Create one durable task in the cross-session task store. For \
                 work with prerequisites, create all task nodes first, then \
                 add dependencies with update_task using the exact ids this \
                 tool returns. Tasks persist across sessions; use todo_write \
                 for the current session's working plan instead.",
            ),
            store: TaskStore::new(root),
        }
    }
}

#[async_trait]
impl TypedTool for CreateTask {
    type Args = CreateTaskArgs;
    type Prepared = PreparedCreateTask;
    type Output = CreateTaskOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        args: CreateTaskArgs,
        _canonical_input: CanonicalToolInput,
        _ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        let subject = args.subject.trim().to_string();
        if subject.is_empty() {
            return Err(ToolOutput::failure(
                ToolErrorKind::InvalidArguments,
                "subject must not be empty",
            ));
        }
        // Rebuilt so hooks and fingerprints see the normalized call: the
        // trimmed subject, and an absent description instead of an empty one.
        let mut canonical = serde_json::json!({ "subject": subject });
        if !args.description.is_empty() {
            canonical["description"] = serde_json::Value::String(args.description.clone());
        }
        let display = ToolDisplay::new(format!(
            "Create task: {}",
            flatten(&subject, SUBJECT_DISPLAY_CHARS)
        ));
        Ok(TypedPreparation::new(
            PreparedCreateTask {
                subject,
                description: args.description,
            },
            CanonicalToolInput::new(canonical),
            task_write_checks(),
            display,
        ))
    }

    async fn run_prepared(
        &self,
        prepared: PreparedCreateTask,
        _ctx: &ToolContext,
    ) -> ToolOutput<CreateTaskOutput> {
        let task = Task::new(generate_task_id(), prepared.subject, prepared.description);
        if let Err(error) = self.store.create_exclusive(&task).await {
            return store_failure(error);
        }
        ToolOutput::success(CreateTaskOutput { task })
    }
}

// ---------------------------------------------------------------------------
// update_task

/// Arguments for [`UpdateTask`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateTaskArgs {
    /// Id of the task to update, e.g. `task_a1b2c3d4`.
    pub task_id: String,
    /// Prerequisite task ids to append to this task's blockers.
    pub add_blocked_by: Vec<String>,
}

/// Result of updating a task's blockers.
#[derive(Debug, Serialize)]
pub struct UpdateTaskOutput {
    /// The stored task after the update.
    pub task: Task,
}

/// Payload retained between preparation and execution.
pub struct PreparedUpdateTask {
    task_id: String,
    add_blocked_by: Vec<String>,
}

/// Appends prerequisites to a pending task. See the [module docs](self).
pub struct UpdateTask {
    definition: ToolDefinition,
    store: TaskStore,
}

impl UpdateTask {
    /// Creates the tool over the per-project tasks root.
    pub fn new(root: PathBuf) -> Self {
        Self {
            definition: definition_for::<UpdateTaskArgs>(
                UPDATE_TASK_TOOL_NAME,
                "Add prerequisite dependencies to a pending, unclaimed task in \
                 the task store. A task cannot be claimed until every task in \
                 its blockers is completed. Dependency cycles are rejected.",
            ),
            store: TaskStore::new(root),
        }
    }
}

#[async_trait]
impl TypedTool for UpdateTask {
    type Args = UpdateTaskArgs;
    type Prepared = PreparedUpdateTask;
    type Output = UpdateTaskOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        args: UpdateTaskArgs,
        _canonical_input: CanonicalToolInput,
        _ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        let task_id = prepare_task_id(&args.task_id)?;
        if args.add_blocked_by.is_empty() {
            return Err(ToolOutput::failure(
                ToolErrorKind::InvalidArguments,
                "add_blocked_by must name at least one task id",
            ));
        }
        let mut add_blocked_by = Vec::with_capacity(args.add_blocked_by.len());
        for dep in &args.add_blocked_by {
            let dep = prepare_task_id(dep)?;
            if dep == task_id {
                return Err(ToolOutput::failure(
                    "dependency_cycle",
                    "a task cannot block on itself",
                ));
            }
            if !add_blocked_by.contains(&dep) {
                add_blocked_by.push(dep);
            }
        }
        let canonical = CanonicalToolInput::new(serde_json::json!({
            "task_id": task_id,
            "add_blocked_by": add_blocked_by,
        }));
        let display = ToolDisplay::new(format!(
            "Update task: {task_id} (+{} blockers)",
            add_blocked_by.len()
        ));
        Ok(TypedPreparation::new(
            PreparedUpdateTask {
                task_id,
                add_blocked_by,
            },
            canonical,
            task_write_checks(),
            display,
        ))
    }

    async fn run_prepared(
        &self,
        prepared: PreparedUpdateTask,
        _ctx: &ToolContext,
    ) -> ToolOutput<UpdateTaskOutput> {
        // Plain read-modify-write; see the store's concurrency note.
        let mut task = match self.store.load(&prepared.task_id).await {
            Ok(task) => task,
            Err(error) => return store_failure(error),
        };
        if task.status != TaskStatus::Pending || task.owner.is_some() {
            return ToolOutput::failure(
                "wrong_status",
                format!(
                    "only a pending, unclaimed task can gain blockers; `{}` is {} (owner: {})",
                    task.id,
                    task.status.as_str(),
                    task.owner.as_deref().unwrap_or("none"),
                ),
            );
        }
        // Every referent must exist; pointing at a completed task is fine
        // (the edge is already satisfied).
        for dep in &prepared.add_blocked_by {
            if let Err(error) = self.store.load(dep).await {
                return store_failure(match error {
                    TaskStoreError::NotFound(id) => TaskStoreError::NotFound(format!(
                        "{id}` referenced by add_blocked_by; create it first or drop the edge (target task `{}",
                        prepared.task_id,
                    )),
                    other => other,
                });
            }
        }
        let all = match self.store.list().await {
            Ok(all) => all,
            Err(error) => return store_failure(error),
        };
        let adjacency = all
            .iter()
            .map(|task| (task.id.clone(), task.blocked_by.clone()))
            .collect();
        if would_cycle(&adjacency, &task.id, &prepared.add_blocked_by) {
            return ToolOutput::failure(
                "dependency_cycle",
                "adding these blockers would create a dependency cycle",
            );
        }
        for dep in prepared.add_blocked_by {
            if !task.blocked_by.contains(&dep) {
                task.blocked_by.push(dep);
            }
        }
        if let Err(error) = self.store.save(&task).await {
            return store_failure(error);
        }
        ToolOutput::success(UpdateTaskOutput { task })
    }
}

// ---------------------------------------------------------------------------
// claim_task

/// Arguments for [`ClaimTask`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimTaskArgs {
    /// Id of the task to claim, e.g. `task_a1b2c3d4`.
    pub task_id: String,
}

/// Result of claiming a task.
#[derive(Debug, Serialize)]
pub struct ClaimTaskOutput {
    /// The stored task after the claim.
    pub task: Task,
}

/// Payload retained between preparation and execution.
pub struct PreparedClaimTask {
    task_id: String,
}

/// Claims a pending task whose blockers are all completed. See the
/// [module docs](self).
pub struct ClaimTask {
    definition: ToolDefinition,
    store: TaskStore,
}

impl ClaimTask {
    /// Creates the tool over the per-project tasks root.
    pub fn new(root: PathBuf) -> Self {
        Self {
            definition: definition_for::<ClaimTaskArgs>(
                CLAIM_TASK_TOOL_NAME,
                "Claim a pending task from the task store to start working on \
                 it. The claim is refused while any blocker is not completed. \
                 Claiming marks the task in_progress and records the owner.",
            ),
            store: TaskStore::new(root),
        }
    }
}

#[async_trait]
impl TypedTool for ClaimTask {
    type Args = ClaimTaskArgs;
    type Prepared = PreparedClaimTask;
    type Output = ClaimTaskOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        args: ClaimTaskArgs,
        _canonical_input: CanonicalToolInput,
        _ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        let task_id = prepare_task_id(&args.task_id)?;
        let canonical = CanonicalToolInput::new(serde_json::json!({ "task_id": task_id }));
        let display = ToolDisplay::new(format!("Claim task: {task_id}"));
        Ok(TypedPreparation::new(
            PreparedClaimTask { task_id },
            canonical,
            task_write_checks(),
            display,
        ))
    }

    async fn run_prepared(
        &self,
        prepared: PreparedClaimTask,
        _ctx: &ToolContext,
    ) -> ToolOutput<ClaimTaskOutput> {
        let mut task = match self.store.load(&prepared.task_id).await {
            Ok(task) => task,
            Err(error) => return store_failure(error),
        };
        if task.status != TaskStatus::Pending {
            return ToolOutput::failure(
                "wrong_status",
                format!(
                    "only a pending task can be claimed; `{}` is {} (owner: {})",
                    task.id,
                    task.status.as_str(),
                    task.owner.as_deref().unwrap_or("none"),
                ),
            );
        }
        let all = match self.store.list().await {
            Ok(all) => all,
            Err(error) => return store_failure(error),
        };
        let by_id: std::collections::BTreeMap<String, Task> = all
            .into_iter()
            .map(|task| (task.id.clone(), task))
            .collect();
        // A blocker whose file is missing counts as not completed: the edge
        // was recorded on purpose, and silently dropping it would start work
        // whose prerequisite nobody finished.
        let unmet: Vec<&str> = task
            .blocked_by
            .iter()
            .filter(|dep| {
                !by_id
                    .get(dep.as_str())
                    .is_some_and(|blocker| blocker.status == TaskStatus::Completed)
            })
            .map(String::as_str)
            .collect();
        if !unmet.is_empty() {
            return ToolOutput::failure(
                "not_claimable",
                format!(
                    "task `{}` is blocked by incomplete tasks: {}",
                    task.id,
                    unmet.join(", "),
                ),
            );
        }
        task.owner = Some(TASK_OWNER.to_string());
        task.status = TaskStatus::InProgress;
        if let Err(error) = self.store.save(&task).await {
            return store_failure(error);
        }
        ToolOutput::success(ClaimTaskOutput { task })
    }
}

// ---------------------------------------------------------------------------
// complete_task

/// Arguments for [`CompleteTask`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompleteTaskArgs {
    /// Id of the task to complete, e.g. `task_a1b2c3d4`.
    pub task_id: String,
}

/// Result of completing a task.
#[derive(Debug, Serialize)]
pub struct CompleteTaskOutput {
    /// The stored task after completion.
    pub task: Task,
    /// Pending tasks this completion made claimable.
    pub unlocked: Vec<TaskSummary>,
}

/// Payload retained between preparation and execution.
pub struct PreparedCompleteTask {
    task_id: String,
}

/// Completes an in-progress task and reports what it unblocked. See the
/// [module docs](self).
pub struct CompleteTask {
    definition: ToolDefinition,
    store: TaskStore,
}

impl CompleteTask {
    /// Creates the tool over the per-project tasks root.
    pub fn new(root: PathBuf) -> Self {
        Self {
            definition: definition_for::<CompleteTaskArgs>(
                COMPLETE_TASK_TOOL_NAME,
                "Mark an in-progress task in the task store as completed. The \
                 result lists the pending tasks this completion unblocked, so \
                 the next claim is obvious.",
            ),
            store: TaskStore::new(root),
        }
    }
}

#[async_trait]
impl TypedTool for CompleteTask {
    type Args = CompleteTaskArgs;
    type Prepared = PreparedCompleteTask;
    type Output = CompleteTaskOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        args: CompleteTaskArgs,
        _canonical_input: CanonicalToolInput,
        _ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        let task_id = prepare_task_id(&args.task_id)?;
        let canonical = CanonicalToolInput::new(serde_json::json!({ "task_id": task_id }));
        let display = ToolDisplay::new(format!("Complete task: {task_id}"));
        Ok(TypedPreparation::new(
            PreparedCompleteTask { task_id },
            canonical,
            task_write_checks(),
            display,
        ))
    }

    async fn run_prepared(
        &self,
        prepared: PreparedCompleteTask,
        _ctx: &ToolContext,
    ) -> ToolOutput<CompleteTaskOutput> {
        let mut task = match self.store.load(&prepared.task_id).await {
            Ok(task) => task,
            Err(error) => return store_failure(error),
        };
        if task.status != TaskStatus::InProgress {
            return ToolOutput::failure(
                "wrong_status",
                format!(
                    "only an in_progress task can be completed; `{}` is {}; claim it first",
                    task.id,
                    task.status.as_str(),
                ),
            );
        }
        // Owner survives completion as a record of who did the work.
        task.status = TaskStatus::Completed;
        if let Err(error) = self.store.save(&task).await {
            return store_failure(error);
        }
        // Fresh listing (it includes the save above) to report what this
        // completion made claimable.
        let all = match self.store.list().await {
            Ok(all) => all,
            Err(error) => return store_failure(error),
        };
        let by_id: std::collections::BTreeMap<String, Task> = all
            .into_iter()
            .map(|task| (task.id.clone(), task))
            .collect();
        let unlocked: Vec<TaskSummary> = by_id
            .values()
            .filter(|candidate| {
                candidate.status == TaskStatus::Pending
                    && candidate.blocked_by.contains(&task.id)
                    && deps_satisfied(candidate, &by_id)
            })
            .map(TaskSummary::from)
            .collect();
        ToolOutput::success(CompleteTaskOutput { task, unlocked })
    }
}

// ---------------------------------------------------------------------------
// get_task

/// Arguments for [`GetTask`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetTaskArgs {
    /// Id of the task to read, e.g. `task_a1b2c3d4`.
    pub task_id: String,
}

/// Result of reading one task.
#[derive(Debug, Serialize)]
pub struct GetTaskOutput {
    /// The stored task, description included.
    pub task: Task,
}

/// Payload retained between preparation and execution.
pub struct PreparedGetTask {
    task_id: String,
}

/// Reads one task in full. See the [module docs](self).
pub struct GetTask {
    definition: ToolDefinition,
    store: TaskStore,
}

impl GetTask {
    /// Creates the tool over the per-project tasks root.
    pub fn new(root: PathBuf) -> Self {
        Self {
            definition: definition_for::<GetTaskArgs>(
                GET_TASK_TOOL_NAME,
                "Read one task from the task store in full, including its \
                 description — the details list_tasks omits.",
            ),
            store: TaskStore::new(root),
        }
    }
}

#[async_trait]
impl TypedTool for GetTask {
    type Args = GetTaskArgs;
    type Prepared = PreparedGetTask;
    type Output = GetTaskOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        args: GetTaskArgs,
        _canonical_input: CanonicalToolInput,
        _ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        let task_id = prepare_task_id(&args.task_id)?;
        // Canonicalized at call time so the permission check names the real
        // file (mirroring load_memory); a missing file is an unknown id.
        let resolved = std::fs::canonicalize(self.store.task_path(&task_id)).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ToolOutput::failure(
                    "task_not_found",
                    format!(
                        "no task with id `{task_id}`; {} tasks stored — call list_tasks to see them",
                        known_task_ids(self.store.root()).len(),
                    ),
                )
            } else {
                ToolOutput::failure(
                    ToolErrorKind::ToolError,
                    format!("failed to resolve task `{task_id}`: {error}"),
                )
            }
        })?;
        let canonical_path = CanonicalPath::from_absolute(&resolved)
            .map_err(|error| ToolOutput::failure(ToolErrorKind::ToolError, error.to_string()))?;
        let canonical = CanonicalToolInput::new(serde_json::json!({ "task_id": task_id }));
        let display = ToolDisplay::new(format!("Get task: {task_id}"));
        Ok(TypedPreparation::new(
            PreparedGetTask { task_id },
            canonical,
            NonEmptyVec::new(PermissionCheckSpec::new(PermissionTarget::Read(
                canonical_path,
            ))),
            display,
        ))
    }

    async fn run_prepared(
        &self,
        prepared: PreparedGetTask,
        _ctx: &ToolContext,
    ) -> ToolOutput<GetTaskOutput> {
        match self.store.load(&prepared.task_id).await {
            Ok(task) => ToolOutput::success(GetTaskOutput { task }),
            Err(error) => store_failure(error),
        }
    }

    fn result_retention(
        &self,
        _args: &serde_json::Value,
        output: &ToolOutput,
    ) -> ToolResultRetention {
        // The disk store is the authority and one call away, so a successful
        // read is safe to slim later; failures may carry evidence.
        if output.ok && !output.truncated {
            ToolResultRetention::Slimmable
        } else {
            ToolResultRetention::Verbatim
        }
    }
}

// ---------------------------------------------------------------------------
// list_tasks

/// Arguments for [`ListTasks`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListTasksArgs {}

/// Result of listing the store.
#[derive(Debug, Serialize)]
pub struct ListTasksOutput {
    /// Every stored task, id-sorted, without descriptions.
    pub tasks: Vec<TaskSummary>,
}

/// Payload retained between preparation and execution.
pub struct PreparedListTasks;

/// Lists the task store. See the [module docs](self).
pub struct ListTasks {
    definition: ToolDefinition,
    store: TaskStore,
}

impl ListTasks {
    /// Creates the tool over the per-project tasks root.
    pub fn new(root: PathBuf) -> Self {
        Self {
            definition: definition_for::<ListTasksArgs>(
                LIST_TASKS_TOOL_NAME,
                "List every task in the cross-session task store: id, subject, \
                 status, owner, and blockers. Descriptions are omitted — read \
                 one task in full with get_task.",
            ),
            store: TaskStore::new(root),
        }
    }
}

#[async_trait]
impl TypedTool for ListTasks {
    type Args = ListTasksArgs;
    type Prepared = PreparedListTasks;
    type Output = ListTasksOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        _args: ListTasksArgs,
        canonical_input: CanonicalToolInput,
        _ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        // Lexical only: the root is absolute by construction and may not
        // exist yet — an empty store lists as empty rather than failing.
        let canonical_path = CanonicalPath::from_absolute(self.store.root())
            .map_err(|error| ToolOutput::failure(ToolErrorKind::ToolError, error.to_string()))?;
        Ok(TypedPreparation::new(
            PreparedListTasks,
            canonical_input,
            NonEmptyVec::new(PermissionCheckSpec::new(PermissionTarget::Read(
                canonical_path,
            ))),
            ToolDisplay::new("List tasks"),
        ))
    }

    async fn run_prepared(
        &self,
        _prepared: PreparedListTasks,
        _ctx: &ToolContext,
    ) -> ToolOutput<ListTasksOutput> {
        match self.store.list().await {
            Ok(tasks) => ToolOutput::success(ListTasksOutput {
                tasks: tasks.iter().map(TaskSummary::from).collect(),
            }),
            Err(error) => store_failure(error),
        }
    }

    fn result_retention(
        &self,
        _args: &serde_json::Value,
        output: &ToolOutput,
    ) -> ToolResultRetention {
        if output.ok && !output.truncated {
            ToolResultRetention::Slimmable
        } else {
            ToolResultRetention::Verbatim
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::test_support::TestDir;
    use crate::tool::{Tool, execute_for_test};

    fn seed(root: &std::path::Path, id: &str, status: TaskStatus, blocked_by: &[&str]) {
        std::fs::create_dir_all(root).expect("root");
        let task = Task {
            id: id.to_string(),
            subject: format!("subject of {id}"),
            description: "details".to_string(),
            status,
            owner: None,
            blocked_by: blocked_by.iter().map(|dep| dep.to_string()).collect(),
        };
        std::fs::write(
            root.join(format!("{id}.json")),
            serde_json::to_vec_pretty(&task).expect("serializes"),
        )
        .expect("seed task");
    }

    fn load(root: &std::path::Path, id: &str) -> Task {
        serde_json::from_slice(&std::fs::read(root.join(format!("{id}.json"))).expect("read"))
            .expect("parses")
    }

    #[tokio::test]
    async fn create_task_persists_and_returns_the_generated_id() {
        let tmp = TestDir::new();
        let root = tmp.path().join("tasks");
        let tool = Arc::new(CreateTask::new(root.clone()));

        let output = execute_for_test(
            tool,
            serde_json::json!({ "subject": "  build the parser  ", "description": "d" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");

        assert!(output.ok);
        let task = &output.data.expect("data present")["task"];
        let id = task["id"].as_str().expect("id is text");
        assert!(validate_task_id(id).is_ok(), "{id}");
        assert_eq!(task["subject"], "build the parser");
        assert_eq!(task["status"], "pending");
        let stored = load(&root, id);
        assert_eq!(stored.subject, "build the parser");
        assert_eq!(stored.description, "d");
    }

    #[tokio::test]
    async fn a_blank_subject_is_refused_with_no_side_effects() {
        let tmp = TestDir::new();
        let root = tmp.path().join("tasks");
        let tool = Arc::new(CreateTask::new(root.clone()));

        let output = execute_for_test(
            tool,
            serde_json::json!({ "subject": "   " }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind,
            ToolErrorKind::InvalidArguments
        );
        // Nothing was written, not even the directory.
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn create_task_rebuilds_the_canonical_input() {
        let tmp = TestDir::new();
        let tool = Arc::new(CreateTask::new(tmp.path().join("tasks")));

        let preparation = tool
            .prepare(
                serde_json::json!({ "subject": "  x  ", "description": "" }),
                &PreparationContext::new(),
            )
            .await
            .expect("valid preparation");

        let canonical = preparation.canonical_input().as_value();
        assert_eq!(canonical["subject"], "x");
        // An empty description stays absent so it cannot fork the fingerprint.
        assert!(canonical.get("description").is_none());
        match preparation.checks().first().target() {
            PermissionTarget::TaskWrite => {}
            other => panic!("expected TaskWrite, got {other:?}"),
        }
        assert_eq!(preparation.display().summary(), "Create task: x");
    }

    #[tokio::test]
    async fn update_task_appends_deduplicated_blockers() {
        let tmp = TestDir::new();
        let root = tmp.path().join("tasks");
        seed(&root, "task_aaaaaaaa", TaskStatus::Pending, &[]);
        seed(&root, "task_bbbbbbbb", TaskStatus::Pending, &[]);
        let tool = Arc::new(UpdateTask::new(root.clone()));

        let output = execute_for_test(
            tool,
            serde_json::json!({
                "task_id": "task_bbbbbbbb",
                "add_blocked_by": [" task_aaaaaaaa ", "task_aaaaaaaa"],
            }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");

        assert!(output.ok, "{:?}", output.error);
        assert_eq!(
            load(&root, "task_bbbbbbbb").blocked_by,
            ["task_aaaaaaaa"],
            "trimmed, deduplicated, persisted",
        );
    }

    #[tokio::test]
    async fn update_task_guards_status_reference_and_cycles() {
        let tmp = TestDir::new();
        let root = tmp.path().join("tasks");
        seed(
            &root,
            "task_aaaaaaaa",
            TaskStatus::Pending,
            &["task_bbbbbbbb"],
        );
        seed(&root, "task_bbbbbbbb", TaskStatus::Pending, &[]);
        seed(&root, "task_cccccccc", TaskStatus::InProgress, &[]);
        let tool = Arc::new(UpdateTask::new(root.clone()));

        // Not pending -> wrong_status.
        let wrong = execute_for_test(
            Arc::clone(&tool),
            serde_json::json!({ "task_id": "task_cccccccc", "add_blocked_by": ["task_bbbbbbbb"] }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");
        assert_eq!(wrong.error.expect("error").kind.as_str(), "wrong_status");

        // Missing referent -> task_not_found naming the dependency.
        let missing = execute_for_test(
            Arc::clone(&tool),
            serde_json::json!({ "task_id": "task_bbbbbbbb", "add_blocked_by": ["task_dddddddd"] }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");
        let missing_error = missing.error.expect("error");
        assert_eq!(missing_error.kind.as_str(), "task_not_found");
        assert!(missing_error.message.contains("task_dddddddd"));

        // Transitive cycle: a blocks on b, so b -> a closes the loop.
        let cycle = execute_for_test(
            Arc::clone(&tool),
            serde_json::json!({ "task_id": "task_bbbbbbbb", "add_blocked_by": ["task_aaaaaaaa"] }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");
        assert_eq!(
            cycle.error.expect("error").kind.as_str(),
            "dependency_cycle"
        );

        // Self-loop is rejected at preparation, before any disk access.
        let self_loop = execute_for_test(
            Arc::clone(&tool),
            serde_json::json!({ "task_id": "task_bbbbbbbb", "add_blocked_by": ["task_bbbbbbbb"] }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");
        assert_eq!(
            self_loop.error.expect("error").kind.as_str(),
            "dependency_cycle"
        );

        // Empty list -> invalid_arguments.
        let empty = execute_for_test(
            tool,
            serde_json::json!({ "task_id": "task_bbbbbbbb", "add_blocked_by": [] }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");
        assert_eq!(
            empty.error.expect("error").kind,
            ToolErrorKind::InvalidArguments
        );
    }

    #[tokio::test]
    async fn claim_sets_owner_and_is_refused_while_blocked() {
        let tmp = TestDir::new();
        let root = tmp.path().join("tasks");
        seed(&root, "task_aaaaaaaa", TaskStatus::Pending, &[]);
        seed(
            &root,
            "task_bbbbbbbb",
            TaskStatus::Pending,
            &["task_aaaaaaaa"],
        );
        seed(
            &root,
            "task_cccccccc",
            TaskStatus::Pending,
            &["task_ffffffff"],
        );
        let tool = Arc::new(ClaimTask::new(root.clone()));

        // Blocked by an incomplete task: refused, listing the blocker.
        let blocked = execute_for_test(
            Arc::clone(&tool),
            serde_json::json!({ "task_id": "task_bbbbbbbb" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");
        let blocked_error = blocked.error.expect("error");
        assert_eq!(blocked_error.kind.as_str(), "not_claimable");
        assert!(blocked_error.message.contains("task_aaaaaaaa"));

        // Blocked by a missing file: also refused.
        let dangling = execute_for_test(
            Arc::clone(&tool),
            serde_json::json!({ "task_id": "task_cccccccc" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");
        assert_eq!(
            dangling.error.expect("error").kind.as_str(),
            "not_claimable"
        );

        // Unblocked: claimed, owned, in progress.
        let claimed = execute_for_test(
            Arc::clone(&tool),
            serde_json::json!({ "task_id": "task_aaaaaaaa" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");
        assert!(claimed.ok, "{:?}", claimed.error);
        let stored = load(&root, "task_aaaaaaaa");
        assert_eq!(stored.status, TaskStatus::InProgress);
        assert_eq!(stored.owner.as_deref(), Some(TASK_OWNER));

        // Claiming again: wrong_status reporting the owner.
        let again = execute_for_test(
            tool,
            serde_json::json!({ "task_id": "task_aaaaaaaa" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");
        let again_error = again.error.expect("error");
        assert_eq!(again_error.kind.as_str(), "wrong_status");
        assert!(again_error.message.contains("agent"));
    }

    #[tokio::test]
    async fn complete_reports_exactly_what_it_unblocked() {
        let tmp = TestDir::new();
        let root = tmp.path().join("tasks");
        seed(&root, "task_aaaaaaaa", TaskStatus::InProgress, &[]);
        // b waits only on a: unlocked. c waits on a and the pending d: not.
        seed(
            &root,
            "task_bbbbbbbb",
            TaskStatus::Pending,
            &["task_aaaaaaaa"],
        );
        seed(
            &root,
            "task_cccccccc",
            TaskStatus::Pending,
            &["task_aaaaaaaa", "task_dddddddd"],
        );
        seed(&root, "task_dddddddd", TaskStatus::Pending, &[]);
        let tool = Arc::new(CompleteTask::new(root.clone()));

        let output = execute_for_test(
            Arc::clone(&tool),
            serde_json::json!({ "task_id": "task_aaaaaaaa" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");

        assert!(output.ok, "{:?}", output.error);
        let data = output.data.expect("data present");
        assert_eq!(data["task"]["status"], "completed");
        let unlocked: Vec<&str> = data["unlocked"]
            .as_array()
            .expect("unlocked is a list")
            .iter()
            .map(|task| task["id"].as_str().expect("id is text"))
            .collect();
        assert_eq!(unlocked, ["task_bbbbbbbb"]);

        // Completing a pending task is refused.
        let pending = execute_for_test(
            tool,
            serde_json::json!({ "task_id": "task_dddddddd" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");
        assert_eq!(pending.error.expect("error").kind.as_str(), "wrong_status");
    }

    #[tokio::test]
    async fn get_task_returns_the_description_and_reads_are_read_checks() {
        let tmp = TestDir::new();
        let root = tmp.path().join("tasks");
        seed(&root, "task_aaaaaaaa", TaskStatus::Pending, &[]);

        let get = Arc::new(GetTask::new(root.clone()));
        let preparation = Arc::clone(&get)
            .prepare(
                serde_json::json!({ "task_id": "task_aaaaaaaa" }),
                &PreparationContext::new(),
            )
            .await
            .expect("valid preparation");
        match preparation.checks().first().target() {
            PermissionTarget::Read(path) => {
                assert!(
                    path.as_str().ends_with("task_aaaaaaaa.json"),
                    "{}",
                    path.as_str()
                );
            }
            other => panic!("expected a Read target, got {other:?}"),
        }
        assert_eq!(preparation.display().summary(), "Get task: task_aaaaaaaa");

        let output = execute_for_test(
            get,
            serde_json::json!({ "task_id": "task_aaaaaaaa" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");
        assert!(output.ok);
        assert_eq!(output.data.expect("data")["task"]["description"], "details");

        let list = Arc::new(ListTasks::new(root.clone()));
        let preparation = Arc::clone(&list)
            .prepare(serde_json::json!({}), &PreparationContext::new())
            .await
            .expect("valid preparation");
        match preparation.checks().first().target() {
            PermissionTarget::Read(path) => {
                assert_eq!(std::path::Path::new(path.as_str()), root);
            }
            other => panic!("expected a Read target, got {other:?}"),
        }

        let output = execute_for_test(list, serde_json::json!({}), &ToolContext::new())
            .await
            .expect("no harness error");
        assert!(output.ok);
        let tasks = output.data.expect("data")["tasks"]
            .as_array()
            .expect("tasks is a list")
            .clone();
        assert_eq!(tasks.len(), 1);
        // The listing omits descriptions.
        assert!(tasks[0].get("description").is_none());
    }

    #[tokio::test]
    async fn unknown_ids_fail_at_preparation_pointing_at_list_tasks() {
        let tmp = TestDir::new();
        let root = tmp.path().join("tasks");
        seed(&root, "task_aaaaaaaa", TaskStatus::Pending, &[]);
        let tool = Arc::new(GetTask::new(root.clone()));

        let Err(output) = tool
            .prepare(
                serde_json::json!({ "task_id": "task_ffffffff" }),
                &PreparationContext::new(),
            )
            .await
        else {
            panic!("unknown id must refuse preparation");
        };
        let error = output.error.expect("error present");
        assert_eq!(error.kind.as_str(), "task_not_found");
        assert!(error.message.contains("list_tasks"));
    }

    #[tokio::test]
    async fn listing_an_empty_store_succeeds_and_reads_are_slimmable() {
        let tmp = TestDir::new();
        let tool = Arc::new(ListTasks::new(tmp.path().join("missing")));

        let output = execute_for_test(
            Arc::clone(&tool),
            serde_json::json!({}),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");

        assert!(output.ok);
        assert_eq!(
            output.data.as_ref().expect("data")["tasks"]
                .as_array()
                .expect("list")
                .len(),
            0
        );
        // Successful reads are acknowledgements of harness-queryable state.
        let erased = ToolOutput {
            ok: true,
            data: output.data.clone(),
            error: None,
            truncated: false,
        };
        assert_eq!(
            TypedTool::result_retention(&*tool, &serde_json::json!({}), &erased),
            ToolResultRetention::Slimmable
        );
        let failed: ToolOutput = ToolOutput::failure("tool_error", "boom");
        assert_eq!(
            TypedTool::result_retention(&*tool, &serde_json::json!({}), &failed),
            ToolResultRetention::Verbatim
        );
    }

    #[tokio::test]
    async fn invalid_ids_are_rejected_before_touching_the_filesystem() {
        let tool = Arc::new(ClaimTask::new(PathBuf::from("/does/not/exist")));

        let Err(output) = tool
            .prepare(
                serde_json::json!({ "task_id": "../escape" }),
                &PreparationContext::new(),
            )
            .await
        else {
            panic!("invalid id must refuse preparation");
        };
        assert_eq!(
            output.error.expect("error present").kind,
            ToolErrorKind::InvalidArguments
        );
    }
}
