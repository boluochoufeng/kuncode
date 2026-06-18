//! Task-plan domain model and the per-session plan store.
//!
//! The plan is the data behind the `todo_write` tool: a flat, ordered list
//! the model overwrites wholesale to keep a long task on track. It lives on the
//! [`AgentSession`](crate::session::AgentSession) — like the permission state —
//! and the tool reaches it through a [`TodoHandle`] carried on the
//! [`ToolContext`](crate::tool::ToolContext), so the tool itself stays stateless
//! and shareable across sessions.

use std::sync::{Arc, Mutex, PoisonError};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Status of one plan item. Serialized `snake_case`, matching the schema shown
/// to the model and the `snake_case` tagging used elsewhere (e.g.
/// [`EventKind`](crate::observer::EventKind)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Not started yet.
    Pending,
    /// Being worked on now. At most one item may be in this state (see
    /// [`TodoList::replace`]).
    InProgress,
    /// Finished.
    Completed,
}

/// One task in the plan.
///
/// Under whole-list overwrite the array position *is* the order, so an item
/// needs no stable id. Carries both an imperative `content` and a present-tense
/// `active_form` so a renderer can show "Write tests" in the list but "Writing
/// tests" while it is in progress.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TodoItem {
    /// Imperative description of the task, e.g. `"Write tests for TodoWrite"`.
    pub content: String,
    /// Present-continuous form shown while the task is
    /// [`InProgress`](TodoStatus::InProgress), e.g. `"Writing tests for
    /// TodoWrite"`.
    pub active_form: String,
    /// Current status of the task.
    pub status: TodoStatus,
}

/// Why a proposed plan was rejected. Surfaced to the model as an
/// `invalid_arguments` [`ToolOutput`](crate::tool::ToolOutput) failure so it can
/// fix the list and resubmit — these are model-recoverable, never harness errors.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum TodoError {
    /// More than one item is [`InProgress`](TodoStatus::InProgress). The plan is
    /// meant to focus on a single active task at a time.
    #[error("at most one task may be in_progress at a time, found {0}")]
    MultipleInProgress(usize),
    /// An item's `content` was empty or whitespace.
    #[error("todo {0} has an empty `content`")]
    EmptyContent(usize),
    /// An item's `active_form` was empty or whitespace.
    #[error("todo {0} has an empty `active_form`")]
    EmptyActiveForm(usize),
}

/// The current plan plus a monotonic write counter.
///
/// The [`generation`](Self::generation) lets the runner notice the plan changed
/// without knowing *which* tool changed it: it snapshots the number before a
/// tool call and compares after, staying generic instead of special-casing
/// `todo_write` by name.
#[derive(Clone, Debug, Default)]
pub struct TodoList {
    items: Vec<TodoItem>,
    generation: u64,
}

impl TodoList {
    /// The current plan items, in order.
    pub fn items(&self) -> &[TodoItem] {
        &self.items
    }

    /// Number of successful overwrites so far. Starts at `0`.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Validates `items`, then overwrites the plan and bumps
    /// [`generation`](Self::generation).
    ///
    /// Whole-list overwrite: the new list fully replaces the old (an empty list
    /// clears the plan). On a validation error the plan is left unchanged and
    /// the generation does not advance.
    ///
    /// # Errors
    ///
    /// Returns [`TodoError`] when more than one item is `in_progress`, or any
    /// item has empty `content` / `active_form`.
    pub fn replace(&mut self, items: Vec<TodoItem>) -> Result<(), TodoError> {
        validate(&items)?;
        self.items = items;
        self.generation += 1;
        Ok(())
    }
}

/// Validates a proposed plan without mutating anything.
fn validate(items: &[TodoItem]) -> Result<(), TodoError> {
    for (index, item) in items.iter().enumerate() {
        if item.content.trim().is_empty() {
            return Err(TodoError::EmptyContent(index));
        }
        if item.active_form.trim().is_empty() {
            return Err(TodoError::EmptyActiveForm(index));
        }
    }

    let in_progress = items
        .iter()
        .filter(|item| item.status == TodoStatus::InProgress)
        .count();
    if in_progress > 1 {
        return Err(TodoError::MultipleInProgress(in_progress));
    }

    Ok(())
}

/// A shared handle to one session's [`TodoList`].
///
/// Cloning shares the same underlying list (`Arc`): the runner keeps one clone
/// on the [`AgentSession`](crate::session::AgentSession) and hands another to the
/// tool through the [`ToolContext`](crate::tool::ToolContext), so a write by the
/// tool is visible to the runner. For an *isolated* copy (e.g. cloning a whole
/// session) use [`deep_clone`](Self::deep_clone).
///
/// [`Default`] yields a standalone empty handle attached to no session, so tools
/// and tests that ignore the plan still get a writable target that goes
/// nowhere — mirroring how [`ToolContext`](crate::tool::ToolContext)'s cancel
/// token defaults to one that never fires.
#[derive(Clone, Debug, Default)]
pub struct TodoHandle(Arc<Mutex<TodoList>>);

impl TodoHandle {
    /// Recovers the guard even if a previous holder panicked. The critical
    /// sections here are trivial and panic-free, so poisoning should never
    /// happen; recovering the inner guard keeps this allocation-free of the
    /// `unwrap`/`panic` the library forbids rather than propagating a poison
    /// error no caller could act on.
    fn lock(&self) -> std::sync::MutexGuard<'_, TodoList> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The current write counter. See [`TodoList::generation`].
    pub fn generation(&self) -> u64 {
        self.lock().generation()
    }

    /// A cloned snapshot of the current plan items, in order.
    pub fn snapshot(&self) -> Vec<TodoItem> {
        self.lock().items().to_vec()
    }

    /// Overwrites the plan with `items`. See [`TodoList::replace`].
    ///
    /// # Errors
    ///
    /// Propagates [`TodoError`] from validation; the stored plan is unchanged.
    pub fn replace(&self, items: Vec<TodoItem>) -> Result<(), TodoError> {
        self.lock().replace(items)
    }

    /// An isolated handle whose list starts as a deep copy of this one's.
    ///
    /// Unlike [`Clone`] (which shares the `Arc`), the returned handle has its own
    /// allocation, so later writes to either do not affect the other. Used by
    /// [`AgentSession`](crate::session::AgentSession)'s manual `Clone` to keep
    /// per-session plan isolation.
    pub fn deep_clone(&self) -> Self {
        Self(Arc::new(Mutex::new(self.lock().clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.to_string(),
            active_form: format!("{content}…"),
            status,
        }
    }

    #[test]
    fn replace_overwrites_and_bumps_generation() {
        let mut list = TodoList::default();
        assert_eq!(list.generation(), 0);

        list.replace(vec![item("a", TodoStatus::Pending)])
            .expect("valid plan");
        assert_eq!(list.items().len(), 1);
        assert_eq!(list.generation(), 1);

        // A second write fully replaces the first and advances the generation.
        list.replace(vec![
            item("b", TodoStatus::InProgress),
            item("c", TodoStatus::Pending),
        ])
        .expect("valid plan");
        assert_eq!(list.items().len(), 2);
        assert_eq!(list.items()[0].content, "b");
        assert_eq!(list.generation(), 2);
    }

    #[test]
    fn empty_list_clears_the_plan() {
        let mut list = TodoList::default();
        list.replace(vec![item("a", TodoStatus::Pending)])
            .expect("valid plan");
        // An empty list is a valid "I'm done" overwrite, not an error.
        list.replace(vec![]).expect("clearing is valid");
        assert!(list.items().is_empty());
        assert_eq!(list.generation(), 2);
    }

    #[test]
    fn rejects_more_than_one_in_progress() {
        let mut list = TodoList::default();
        let err = list
            .replace(vec![
                item("a", TodoStatus::InProgress),
                item("b", TodoStatus::InProgress),
            ])
            .expect_err("two in_progress is invalid");
        assert_eq!(err, TodoError::MultipleInProgress(2));
        // The rejected write left the plan and its generation untouched.
        assert!(list.items().is_empty());
        assert_eq!(list.generation(), 0);
    }

    #[test]
    fn rejects_empty_content_or_active_form() {
        let mut list = TodoList::default();
        assert_eq!(
            list.replace(vec![item("   ", TodoStatus::Pending)])
                .expect_err("blank content"),
            TodoError::EmptyContent(0),
        );

        let blank_active = TodoItem {
            content: "do it".to_string(),
            active_form: "  ".to_string(),
            status: TodoStatus::Pending,
        };
        assert_eq!(
            list.replace(vec![blank_active])
                .expect_err("blank active_form"),
            TodoError::EmptyActiveForm(0),
        );
    }

    #[test]
    fn clone_shares_but_deep_clone_isolates() {
        let handle = TodoHandle::default();
        handle
            .replace(vec![item("a", TodoStatus::Pending)])
            .expect("valid");

        // A plain clone shares the allocation: a write through one is seen by
        // the other.
        let shared = handle.clone();
        shared
            .replace(vec![item("b", TodoStatus::Pending)])
            .expect("valid");
        assert_eq!(handle.snapshot()[0].content, "b");

        // A deep clone is isolated: later writes don't cross over.
        let isolated = handle.deep_clone();
        isolated
            .replace(vec![item("c", TodoStatus::Pending)])
            .expect("valid");
        assert_eq!(handle.snapshot()[0].content, "b");
        assert_eq!(isolated.snapshot()[0].content, "c");
    }
}
