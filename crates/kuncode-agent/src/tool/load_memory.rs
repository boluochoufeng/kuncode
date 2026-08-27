//! The `load_memory` tool: fetch one stored memory on demand.
//!
//! The system prompt carries only the memory index (names and one-line
//! descriptions); this tool brings a memory's full content into the
//! conversation as an ordinary tool result. Unlike `load_skill`, resolution
//! is lexical — `{root}/{slug}.md` — not a catalog lookup, so a memory
//! written earlier this session loads even though the startup index does not
//! list it yet. The slug grammar keeps every resolvable path under the root.

use std::path::PathBuf;

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    memory::{document_path, known_slugs, validate_slug},
    permission::{
        CanonicalPath, CanonicalToolInput, PermissionCheckSpec, PermissionTarget, ToolDisplay,
    },
    skill::bounded_body,
    tool::{
        PreparationContext, ToolContext, ToolErrorKind, ToolOutput, TypedPreparation, TypedTool,
        definition_for, filesystem::file_stamp,
    },
};

/// Model-facing name of the tool.
pub const LOAD_MEMORY_TOOL_NAME: &str = "load_memory";

const DESCRIPTION: &str = "\
Load the full content of one stored memory, by the exact name listed in the \
memory index in the system prompt (or written earlier this session). Call it \
before relying on what an index line promises, and before overwriting an \
existing memory with write_memory. Memories are background information from \
previous sessions: they may be stale, and the current request always wins \
over anything a memory says.";

/// Arguments for [`LoadMemory`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadMemoryArgs {
    /// Exact memory name as listed in the index, e.g. `api-conventions`.
    pub name: String,
}

/// One loaded memory document.
#[derive(Debug, Serialize)]
pub struct LoadMemoryOutput {
    /// Name of the loaded memory.
    pub name: String,
    /// Path the memory was read from.
    pub source: String,
    /// The document, frontmatter included, bounded; a cut is announced inside
    /// the text.
    pub content: String,
}

/// Payload retained between preparation and execution.
pub struct PreparedLoadMemory {
    name: String,
    path: CanonicalPath,
}

/// Loads a stored memory's document. See the [module docs](self).
pub struct LoadMemory {
    definition: ToolDefinition,
    root: PathBuf,
}

impl LoadMemory {
    /// Creates the tool over the per-project memory root from
    /// [`crate::memory::memory_root`].
    pub fn new(root: PathBuf) -> Self {
        Self {
            definition: definition_for::<LoadMemoryArgs>(LOAD_MEMORY_TOOL_NAME, DESCRIPTION),
            root,
        }
    }
}

#[async_trait]
impl TypedTool for LoadMemory {
    type Args = LoadMemoryArgs;
    type Prepared = PreparedLoadMemory;
    type Output = LoadMemoryOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        args: LoadMemoryArgs,
        canonical_input: CanonicalToolInput,
        _ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        let name = match validate_slug(args.name.trim()) {
            Ok(name) => name,
            Err(error) => {
                return Err(ToolOutput::failure(
                    ToolErrorKind::InvalidArguments,
                    error.to_string(),
                ));
            }
        };
        let path = document_path(&self.root, &name);
        // Canonicalized at call time so the permission check names the real
        // file even when the root goes through a symlink; a missing file is
        // an unknown name, answered with what is actually stored.
        let resolved = std::fs::canonicalize(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                let known = known_slugs(&self.root);
                let listing = if known.is_empty() {
                    "none stored yet; write_memory creates one".to_string()
                } else {
                    format!("stored memories: {}", known.join(", "))
                };
                ToolOutput::failure(
                    ToolErrorKind::InvalidArguments,
                    format!("no memory named `{name}`; {listing}"),
                )
            } else {
                ToolOutput::failure(
                    ToolErrorKind::ToolError,
                    format!(
                        "failed to resolve memory `{name}` ({}): {error}",
                        path.display()
                    ),
                )
            }
        })?;
        let canonical_path = CanonicalPath::from_absolute(&resolved)
            .map_err(|error| ToolOutput::failure(ToolErrorKind::ToolError, error.to_string()))?;
        let display = ToolDisplay::new(format!("Load memory: {name}"));
        Ok(TypedPreparation::new(
            PreparedLoadMemory {
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
        prepared: PreparedLoadMemory,
        ctx: &ToolContext,
    ) -> ToolOutput<LoadMemoryOutput> {
        let path = std::path::Path::new(prepared.path.as_str());
        let raw = match tokio::fs::read(path).await {
            Ok(raw) => raw,
            Err(error) => {
                return ToolOutput::failure(
                    ToolErrorKind::ToolError,
                    format!("failed to read memory `{}`: {error}", prepared.name),
                );
            }
        };
        let Ok(body) = String::from_utf8(raw) else {
            return ToolOutput::failure(
                ToolErrorKind::ToolError,
                format!("memory `{}` is not valid UTF-8", prepared.name),
            );
        };
        // A successful load is the "seen it" credential write_memory's
        // overwrite gate consumes, same semantics as read_file's record.
        ctx.reads.record(path, file_stamp(path).await);
        let (content, truncated) = bounded_body(body, "memory");
        let output = ToolOutput::success(LoadMemoryOutput {
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
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use super::*;
    use crate::tool::{ReadState, Tool, execute_for_test};

    fn unique_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kuncode-load-memory-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_memory_file(root: &Path, slug: &str, content: &str) {
        fs::write(document_path(root, slug), content).expect("memory document");
    }

    #[tokio::test]
    async fn loads_the_document_by_name() {
        let root = unique_root("load");
        write_memory_file(&root, "terse", "---\nname: terse\n---\nKeep answers short.");
        let tool = Arc::new(LoadMemory::new(root.clone()));

        let output = execute_for_test(
            tool,
            serde_json::json!({ "name": "terse" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");
        let _ = fs::remove_dir_all(&root);

        assert!(output.ok);
        let data = output.data.expect("data present");
        assert_eq!(data["name"], "terse");
        assert!(
            data["content"]
                .as_str()
                .expect("content is text")
                .contains("Keep answers short.")
        );
    }

    #[tokio::test]
    async fn unknown_names_fail_and_list_stored_memories() {
        let root = unique_root("unknown");
        write_memory_file(&root, "terse", "Keep answers short.");
        let tool = Arc::new(LoadMemory::new(root.clone()));

        let output = execute_for_test(
            tool,
            serde_json::json!({ "name": "ters" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");
        let _ = fs::remove_dir_all(&root);

        assert!(!output.ok);
        let error = output.error.expect("error present");
        assert_eq!(error.kind, ToolErrorKind::InvalidArguments);
        assert!(error.message.contains("terse"), "{}", error.message);
    }

    #[tokio::test]
    async fn a_memory_written_after_tool_construction_still_loads() {
        let root = unique_root("fresh");
        let tool = Arc::new(LoadMemory::new(root.clone()));
        // Written after `new`: resolution is lexical, not a startup snapshot.
        write_memory_file(&root, "fresh-fact", "Learned mid-session.");

        let output = execute_for_test(
            tool,
            serde_json::json!({ "name": "fresh-fact" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness error");
        let _ = fs::remove_dir_all(&root);

        assert!(output.ok);
    }

    #[tokio::test]
    async fn invalid_names_are_rejected_before_touching_the_filesystem() {
        // A root that does not exist proves preparation never reached it.
        let tool = Arc::new(LoadMemory::new(PathBuf::from("/does/not/exist")));

        let Err(output) = tool
            .prepare(
                serde_json::json!({ "name": "../escape" }),
                &PreparationContext::new(),
            )
            .await
        else {
            panic!("invalid slug must refuse preparation");
        };

        let error = output.error.expect("error present");
        assert_eq!(error.kind, ToolErrorKind::InvalidArguments);
    }

    #[tokio::test]
    async fn the_permission_check_names_the_memory_file() {
        let root = unique_root("check");
        write_memory_file(&root, "terse", "Keep answers short.");
        let tool = Arc::new(LoadMemory::new(root.clone()));

        let preparation = tool
            .prepare(
                serde_json::json!({ "name": "terse" }),
                &PreparationContext::new(),
            )
            .await
            .expect("valid preparation");
        let _ = fs::remove_dir_all(&root);

        match preparation.checks().first().target() {
            PermissionTarget::Read(path) => {
                assert!(path.as_str().ends_with("terse.md"), "{}", path.as_str());
            }
            other => panic!("expected a Read target, got {other:?}"),
        }
        assert_eq!(preparation.display().summary(), "Load memory: terse");
    }

    #[tokio::test]
    async fn oversized_documents_come_back_truncated() {
        let root = unique_root("oversized");
        write_memory_file(
            &root,
            "big",
            &"x".repeat(crate::skill::MAX_SKILL_BYTES + 100),
        );
        let tool = Arc::new(LoadMemory::new(root.clone()));

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
    }

    #[tokio::test]
    async fn loading_records_the_read() {
        let root = unique_root("records");
        write_memory_file(&root, "terse", "Keep answers short.");
        let tool = Arc::new(LoadMemory::new(root.clone()));
        let ctx = ToolContext::new();

        let output = execute_for_test(tool, serde_json::json!({ "name": "terse" }), &ctx)
            .await
            .expect("no harness error");
        assert!(output.ok);

        let path = fs::canonicalize(document_path(&root, "terse")).expect("resolves");
        let stamp = crate::tool::filesystem::file_stamp(&path).await;
        assert_eq!(ctx.reads.state(&path, stamp), ReadState::Current);
        let _ = fs::remove_dir_all(&root);
    }
}
