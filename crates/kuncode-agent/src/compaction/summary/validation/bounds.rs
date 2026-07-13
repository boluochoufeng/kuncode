use std::collections::BTreeSet;

use super::SummaryError;
use crate::compaction::summary::{CommandSummary, ContinuitySummary, SummaryTodo};

pub(super) const MAX_SUMMARY_JSON_BYTES: usize = 256 * 1_024;
const MAX_GOAL_BYTES: usize = 8 * 1_024;
const MAX_TEXT_BYTES: usize = 4 * 1_024;
const MAX_COMMAND_BYTES: usize = 8 * 1_024;
const MAX_LIST_ITEMS: usize = 64;
const MAX_PATH_ITEMS: usize = 128;
const MAX_SYMBOL_ITEMS: usize = 128;
const MAX_ARTIFACT_REFS: usize = 128;
const ARTIFACT_PREFIX: &str = "tool-result-sha256-";

pub(super) fn validate_summary_bounds(summary: &ContinuitySummary) -> Result<(), SummaryError> {
    field("current_goal", &summary.current_goal, MAX_GOAL_BYTES)?;
    text_list("constraints", &summary.constraints, MAX_LIST_ITEMS)?;
    text_list("decisions", &summary.decisions, MAX_LIST_ITEMS)?;
    text_list("completed_work", &summary.completed_work, MAX_LIST_ITEMS)?;
    field(
        "workspace.working_directory",
        &summary.workspace.working_directory,
        MAX_TEXT_BYTES,
    )?;
    text_list("workspace.files", &summary.workspace.files, MAX_PATH_ITEMS)?;
    text_list(
        "workspace.symbols",
        &summary.workspace.symbols,
        MAX_SYMBOL_ITEMS,
    )?;
    commands(&summary.commands_and_tests)?;
    text_list(
        "unresolved_errors",
        &summary.unresolved_errors,
        MAX_LIST_ITEMS,
    )?;
    todos(&summary.todos)?;
    text_list("next_actions", &summary.next_actions, MAX_LIST_ITEMS)?;
    count(
        "artifact_refs",
        summary.artifact_refs.len(),
        MAX_ARTIFACT_REFS,
    )?;
    for (index, artifact_ref) in summary.artifact_refs.iter().enumerate() {
        field(
            &format!("artifact_refs[{index}]"),
            artifact_ref,
            ARTIFACT_PREFIX.len() + 64,
        )?;
    }
    Ok(())
}

pub(super) fn validate_allowed_artifact_refs(
    artifact_refs: &BTreeSet<String>,
) -> Result<(), SummaryError> {
    count(
        "allowed_artifact_refs",
        artifact_refs.len(),
        MAX_ARTIFACT_REFS,
    )?;
    for artifact_ref in artifact_refs {
        if !is_artifact_id(artifact_ref) {
            return Err(SummaryError::InvalidArtifactRef(artifact_ref.clone()));
        }
    }
    Ok(())
}

pub(super) fn is_artifact_id(value: &str) -> bool {
    let Some(hash) = value.strip_prefix(ARTIFACT_PREFIX) else {
        return false;
    };
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn commands(commands: &[CommandSummary]) -> Result<(), SummaryError> {
    count("commands_and_tests", commands.len(), MAX_LIST_ITEMS)?;
    for (index, command) in commands.iter().enumerate() {
        field(
            &format!("commands_and_tests[{index}].command"),
            &command.command,
            MAX_COMMAND_BYTES,
        )?;
        field(
            &format!("commands_and_tests[{index}].outcome"),
            &command.outcome,
            MAX_TEXT_BYTES,
        )?;
    }
    Ok(())
}

fn todos(todos: &[SummaryTodo]) -> Result<(), SummaryError> {
    count("todos", todos.len(), MAX_LIST_ITEMS)?;
    for (index, todo) in todos.iter().enumerate() {
        field(
            &format!("todos[{index}].content"),
            &todo.content,
            MAX_TEXT_BYTES,
        )?;
    }
    Ok(())
}

fn text_list(field_name: &str, values: &[String], max_items: usize) -> Result<(), SummaryError> {
    count(field_name, values.len(), max_items)?;
    for (index, value) in values.iter().enumerate() {
        field(&format!("{field_name}[{index}]"), value, MAX_TEXT_BYTES)?;
    }
    Ok(())
}

fn count(field: &str, actual: usize, max: usize) -> Result<(), SummaryError> {
    if actual > max {
        Err(SummaryError::TooManyItems {
            field: field.to_string(),
            max,
            actual,
        })
    } else {
        Ok(())
    }
}

fn field(field: &str, value: &str, max: usize) -> Result<(), SummaryError> {
    if value.len() > max {
        Err(SummaryError::FieldTooLarge {
            field: field.to_string(),
            max,
            actual: value.len(),
        })
    } else {
        Ok(())
    }
}
