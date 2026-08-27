//! The `load_skill` tool: fetch one skill document on demand.
//!
//! The system prompt carries only the skill catalog (names and one-line
//! descriptions); this tool brings a skill's full instructions into the
//! conversation as an ordinary tool result, so a document costs tokens only in
//! the sessions that actually use it. Lookup goes through the startup-scanned
//! [`SkillCatalog`] by registered name — the argument is never treated as a
//! path.

use std::sync::Arc;

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    permission::{
        CanonicalPath, CanonicalToolInput, PermissionCheckSpec, PermissionTarget, ToolDisplay,
    },
    skill::{SkillCatalog, bounded_body},
    tool::{
        PreparationContext, ToolContext, ToolErrorKind, ToolOutput, TypedPreparation, TypedTool,
        definition_for,
    },
};

/// Model-facing name of the tool.
pub const LOAD_SKILL_TOOL_NAME: &str = "load_skill";

const DESCRIPTION: &str = "\
Load the full instructions of one skill from the catalog listed in the system \
prompt. Call it with the skill's exact name before doing work the skill \
covers, then follow the returned instructions. The result enters the \
conversation once; do not reload a skill you already loaded this session.";

/// Arguments for [`LoadSkill`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadSkillArgs {
    /// Exact skill name as listed in the catalog, e.g. `code-review`.
    pub name: String,
}

/// One loaded skill document.
#[derive(Debug, Serialize)]
pub struct LoadSkillOutput {
    /// Registered name of the loaded skill.
    pub name: String,
    /// Path the instructions were read from.
    pub source: String,
    /// The document body, bounded; a cut is announced inside the text.
    pub content: String,
}

/// Payload retained between preparation and execution.
pub struct PreparedLoadSkill {
    name: String,
    path: CanonicalPath,
}

/// Loads a catalog skill's document. See the [module docs](self).
pub struct LoadSkill {
    definition: ToolDefinition,
    catalog: Arc<SkillCatalog>,
}

impl LoadSkill {
    /// Creates the tool over the startup-scanned catalog.
    pub fn new(catalog: Arc<SkillCatalog>) -> Self {
        Self {
            definition: definition_for::<LoadSkillArgs>(LOAD_SKILL_TOOL_NAME, DESCRIPTION),
            catalog,
        }
    }

    /// A bounded name list for the not-found message, so a typo gets the model
    /// back on track without a second discovery step.
    fn known_names(&self) -> String {
        self.catalog
            .summaries()
            .iter()
            .map(|summary| summary.name().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[async_trait]
impl TypedTool for LoadSkill {
    type Args = LoadSkillArgs;
    type Prepared = PreparedLoadSkill;
    type Output = LoadSkillOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        args: LoadSkillArgs,
        canonical_input: CanonicalToolInput,
        _ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        let name = args.name.trim().to_string();
        let Some(path) = self.catalog.document_path(&name) else {
            return Err(ToolOutput::failure(
                ToolErrorKind::InvalidArguments,
                format!(
                    "unknown skill `{name}`; available skills: {}",
                    self.known_names()
                ),
            ));
        };
        // Canonicalized at call time so the permission check names the real
        // file even when the scanned root goes through a symlink.
        let resolved = std::fs::canonicalize(path).map_err(|error| {
            ToolOutput::failure(
                ToolErrorKind::ToolError,
                format!(
                    "skill `{name}` document disappeared since startup ({}): {error}",
                    path.display()
                ),
            )
        })?;
        let canonical_path = CanonicalPath::from_absolute(&resolved)
            .map_err(|error| ToolOutput::failure(ToolErrorKind::ToolError, error.to_string()))?;
        let display = ToolDisplay::new(format!("Load skill: {name}"));
        Ok(TypedPreparation::new(
            PreparedLoadSkill {
                name,
                path: canonical_path.clone(),
            },
            canonical_input,
            NonEmptyVec::new(PermissionCheckSpec::new(PermissionTarget::Read(
                canonical_path,
            ))),
            display,
        ))
    }

    async fn run_prepared(
        &self,
        prepared: PreparedLoadSkill,
        _ctx: &ToolContext,
    ) -> ToolOutput<LoadSkillOutput> {
        // Read fresh at execution: the catalog froze the *identity* at startup,
        // but the instructions themselves may have been edited since.
        let raw = match tokio::fs::read(prepared.path.as_str()).await {
            Ok(raw) => raw,
            Err(error) => {
                return ToolOutput::failure(
                    ToolErrorKind::ToolError,
                    format!("failed to read skill `{}`: {error}", prepared.name),
                );
            }
        };
        let Ok(body) = String::from_utf8(raw) else {
            return ToolOutput::failure(
                ToolErrorKind::ToolError,
                format!("skill `{}` is no longer valid UTF-8", prepared.name),
            );
        };
        let (content, truncated) = bounded_body(body, "skill");
        let output = ToolOutput::success(LoadSkillOutput {
            name: prepared.name,
            source: prepared.path.as_str().to_string(),
            content,
        });
        if truncated {
            output.truncated()
        } else {
            output
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::tool::{Tool, execute_for_test};

    fn unique_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kuncode-load-skill-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn catalog_with(root: &PathBuf, name: &str, content: &str) -> Arc<SkillCatalog> {
        let dir = root.join(name);
        fs::create_dir_all(&dir).expect("skill dir");
        fs::write(dir.join("SKILL.md"), content).expect("skill document");
        Arc::new(SkillCatalog::scan(std::slice::from_ref(root)))
    }

    #[tokio::test]
    async fn loads_the_document_by_registered_name() {
        let root = unique_root("load");
        let catalog = catalog_with(&root, "deploy", "---\nname: deploy\n---\nShip it safely.");
        let tool = Arc::new(LoadSkill::new(catalog));

        let output = execute_for_test(
            tool,
            serde_json::json!({ "name": "deploy" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");
        let _ = fs::remove_dir_all(&root);

        assert!(output.ok);
        let data = output.data.expect("data present");
        assert_eq!(data["name"], "deploy");
        assert!(
            data["content"]
                .as_str()
                .expect("content is text")
                .contains("Ship it safely.")
        );
    }

    #[tokio::test]
    async fn unknown_names_fail_and_list_the_catalog() {
        let root = unique_root("unknown");
        let catalog = catalog_with(&root, "deploy", "Ship it.");
        let tool = Arc::new(LoadSkill::new(catalog));

        let output = execute_for_test(
            tool,
            serde_json::json!({ "name": "deplo" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");
        let _ = fs::remove_dir_all(&root);

        assert!(!output.ok);
        let error = output.error.expect("error present");
        assert_eq!(error.kind, ToolErrorKind::InvalidArguments);
        assert!(error.message.contains("deploy"), "{}", error.message);
    }

    #[tokio::test]
    async fn the_permission_check_names_the_document_file() {
        let root = unique_root("check");
        let catalog = catalog_with(&root, "deploy", "Ship it.");
        let tool = Arc::new(LoadSkill::new(catalog));

        let preparation = tool
            .prepare(
                serde_json::json!({ "name": "deploy" }),
                &PreparationContext::new(),
            )
            .await
            .expect("valid preparation");
        let _ = fs::remove_dir_all(&root);

        match preparation.checks().first().target() {
            PermissionTarget::Read(path) => {
                assert!(path.as_str().ends_with("SKILL.md"), "{}", path.as_str());
            }
            other => panic!("expected a Read target, got {other:?}"),
        }
        assert_eq!(preparation.display().summary(), "Load skill: deploy");
    }

    #[tokio::test]
    async fn a_document_deleted_after_startup_is_a_recoverable_failure() {
        let root = unique_root("deleted");
        let catalog = catalog_with(&root, "deploy", "Ship it.");
        let tool = Arc::new(LoadSkill::new(catalog));
        let _ = fs::remove_dir_all(&root);

        let output = execute_for_test(
            tool,
            serde_json::json!({ "name": "deploy" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind,
            ToolErrorKind::ToolError
        );
    }

    #[tokio::test]
    async fn oversized_documents_come_back_truncated() {
        let root = unique_root("oversized");
        let body = "x".repeat(crate::skill::MAX_SKILL_BYTES + 100);
        let catalog = catalog_with(&root, "big", &body);
        let tool = Arc::new(LoadSkill::new(catalog));

        let output = execute_for_test(
            tool,
            serde_json::json!({ "name": "big" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");
        let _ = fs::remove_dir_all(&root);

        assert!(output.ok);
        assert!(output.truncated);
        let content = output.data.expect("data present")["content"]
            .as_str()
            .expect("content is text")
            .to_string();
        assert!(content.contains("[truncated:"), "cut not announced");
    }
}
