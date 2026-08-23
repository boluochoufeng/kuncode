//! The `write_memory` tool: save one memory for future sessions.
//!
//! Mirrors `write_file` in shape — wholesale replacement, blind-overwrite
//! gate, diff preview at approval — but targets the per-project memory root
//! outside the workspace. There is no path argument: the slug grammar alone
//! decides the file, which is what confines every write to the root without a
//! workspace-style resolver.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    memory::{MemoryKind, document_path, render_document, validate_slug},
    permission::{
        CanonicalPath, CanonicalToolInput, ChangePreview, PermissionCheckSpec, PermissionTarget,
        ToolDisplay,
    },
    tool::{
        FileStamp, PreparationContext, ReadLedger, ReadState, ToolContext, ToolErrorKind,
        ToolOutput, TypedPreparation, TypedTool, definition_for,
        filesystem::{OpenError, file_stamp, write_no_follow},
    },
};

/// Model-facing name of the tool.
pub const WRITE_MEMORY_TOOL_NAME: &str = "write_memory";

const DESCRIPTION: &str = "\
Save one memory for future sessions in this project. Use it when the user \
states a durable preference, corrects a mistake worth not repeating, or you \
learn a project fact future sessions will need — not for task state, scratch \
notes, or anything already written down in the project itself. Prefer \
updating an existing memory over creating a near-duplicate. Writing an \
existing name replaces that memory wholesale, and is refused unless \
load_memory returned it this session — load first, then write the version \
that carries forward what still holds. New memories enter the system-prompt \
index on the next session start.";

/// Arguments accepted by the [`WriteMemory`] tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteMemoryArgs {
    /// Memory name and filename: lowercase letters, digits and hyphens only,
    /// at most 64 characters, e.g. `api-conventions`.
    pub name: String,
    /// One line stating when this memory applies. It becomes the index entry
    /// future sessions decide from, so make it self-explanatory.
    pub description: String,
    /// Kind of memory: `user` (durable facts about the user), `feedback`
    /// (guidance on how to work), `project` (facts and decisions not in the
    /// repository), or `reference` (pointers to external resources). Omit
    /// when none fits.
    #[serde(default, rename = "type")]
    pub kind: Option<MemoryKind>,
    /// Complete content of the memory (Markdown). Replaces the stored body
    /// wholesale — pass everything it should end up containing.
    pub body: String,
}

/// Result of storing a memory.
#[derive(Debug, Serialize)]
pub struct WriteMemoryOutput {
    /// Name of the stored memory.
    pub name: String,
    /// Absolute path of the stored document.
    pub path: String,
    /// Number of UTF-8 bytes written, frontmatter included.
    pub bytes: usize,
    /// `false` when an existing memory was replaced.
    pub created: bool,
}

/// Payload retained between preparation and execution.
pub struct PreparedWriteMemory {
    slug: String,
    path: PathBuf,
    /// Rendered frontmatter plus body, built at preparation so the approval
    /// preview shows exactly the bytes execution writes.
    document: String,
}

/// Stores memories under the per-project root. See the [module docs](self).
pub struct WriteMemory {
    definition: ToolDefinition,
    root: PathBuf,
}

impl WriteMemory {
    /// Creates the tool over the per-project memory root from
    /// [`crate::memory::memory_root`].
    pub fn new(root: PathBuf) -> Self {
        Self {
            definition: definition_for::<WriteMemoryArgs>(WRITE_MEMORY_TOOL_NAME, DESCRIPTION),
            root,
        }
    }
}

#[async_trait]
impl TypedTool for WriteMemory {
    type Args = WriteMemoryArgs;
    type Prepared = PreparedWriteMemory;
    type Output = WriteMemoryOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        args: WriteMemoryArgs,
        _canonical_input: CanonicalToolInput,
        ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        let slug = match validate_slug(args.name.trim()) {
            Ok(slug) => slug,
            Err(error) => {
                return Err(ToolOutput::failure(
                    ToolErrorKind::InvalidArguments,
                    error.to_string(),
                ));
            }
        };
        if args.description.trim().is_empty() {
            return Err(ToolOutput::failure(
                ToolErrorKind::InvalidArguments,
                "description must not be empty; it is the index line future sessions decide from",
            ));
        }
        if args.body.trim().is_empty() {
            return Err(ToolOutput::failure(
                ToolErrorKind::InvalidArguments,
                "body must not be empty; a memory with nothing to say should not exist",
            ));
        }
        let path = document_path(&self.root, &slug);
        // Lexical canonicalization only: the file may not exist yet, and the
        // slug grammar already rules out anything that could leave the root.
        let canonical_path = CanonicalPath::from_absolute(&path)
            .map_err(|error| ToolOutput::failure(ToolErrorKind::ToolError, error.to_string()))?;
        let document = render_document(&slug, &args.description, args.kind, &args.body);
        // Refused before the approval prompt for the same reason write_file
        // does it: being asked to approve a call that then fails anyway
        // teaches that the prompt is not worth reading. Execution asks again.
        if let Some(refusal) = refuse_unseen_overwrite(&ctx.reads, &path).await {
            return Err(refusal);
        }
        // Rebuilt so hooks and fingerprints see the normalized call: the
        // trimmed slug, and an absent kind instead of an explicit null.
        let mut canonical = serde_json::json!({
            "name": slug,
            "description": args.description,
            "body": args.body,
        });
        if let Some(kind) = args.kind {
            canonical["type"] = serde_json::Value::String(kind.as_str().to_string());
        }
        let canonical_input = CanonicalToolInput::new(canonical);
        let display = ToolDisplay::new(format!("Write memory: {slug}"))
            .with_preview(preview_against_disk(&path, &document).await);
        Ok(TypedPreparation::new(
            PreparedWriteMemory {
                slug,
                path,
                document,
            },
            canonical_input,
            NonEmptyVec::new(PermissionCheckSpec::new(PermissionTarget::Edit(
                canonical_path,
            ))),
            display,
        ))
        // revalidate_prepared stays defaulted: there is no user path to
        // re-resolve, a final-component symlink swap is refused inside
        // write_no_follow's open, and content changes during the approval
        // window are caught by the second overwrite check below.
    }

    async fn run_prepared(
        &self,
        prepared: PreparedWriteMemory,
        ctx: &ToolContext,
    ) -> ToolOutput<WriteMemoryOutput> {
        let PreparedWriteMemory {
            slug,
            path,
            document,
        } = prepared;
        // Second half of the preparation-time check, against the file as it
        // stands now: it could change while the approval prompt was open.
        if let Some(refusal) = refuse_unseen_overwrite(&ctx.reads, &path).await {
            return refusal;
        }
        let created = tokio::fs::symlink_metadata(&path).await.is_err();
        // Created lazily at first write, not at startup: preparation must stay
        // side-effect-free, and most sessions never store a memory.
        if let Err(error) = tokio::fs::create_dir_all(&self.root).await {
            return ToolOutput::failure(
                "write",
                format!(
                    "failed to create memory directory `{}`: {error}",
                    self.root.display()
                ),
            );
        }
        if let Err(error) = write_no_follow(&path, document.as_bytes()).await {
            return match error {
                OpenError::Symlink => ToolOutput::failure(
                    "symlink_not_followed",
                    format!(
                        "`{}` is a symlink and was not followed; store the memory elsewhere",
                        path.display()
                    ),
                ),
                OpenError::Io(error) => ToolOutput::failure(
                    "write",
                    format!("failed to write memory `{}`: {error}", path.display()),
                ),
            };
        }
        // Supplying the document whole is knowing it whole, so this counts as
        // a reading — without it, updating the same memory twice in one
        // session would be refused the second time.
        ctx.reads.record(&path, file_stamp(&path).await);
        ToolOutput::success(WriteMemoryOutput {
            name: slug,
            path: path.display().to_string(),
            bytes: document.len(),
            created,
        })
    }
}

/// Diffs what is stored against what is about to replace it; a missing or
/// unreadable file previews as nothing rather than as an error, because this
/// runs before authorization and must not turn an allowed call into a failure.
async fn preview_against_disk(path: &Path, document: &str) -> Option<ChangePreview> {
    let existing = tokio::fs::read(path).await.ok()?;
    let existing = String::from_utf8(existing).ok()?;
    ChangePreview::between(&existing, document)
}

/// Refuses to replace a memory whose current contents the session has not
/// seen.
///
/// Same invariant as `write_file`'s blind-overwrite gate, restated locally
/// because the refusals must point at `load_memory` — the only reader for
/// these files — not at `read_file`/`edit_file`, which cannot reach them.
/// Run once while preparing and once while executing, which is why it takes
/// the ledger rather than either context.
async fn refuse_unseen_overwrite<D>(reads: &ReadLedger, path: &Path) -> Option<ToolOutput<D>> {
    // No metadata means no file yet, and creating one loses nothing. Symlinks
    // are read without following; `write_no_follow` refuses them at open time
    // with a more accurate message than "you have not loaded this memory".
    let metadata = tokio::fs::symlink_metadata(path).await.ok()?;
    if !metadata.is_file() {
        return None;
    }
    match reads.state(path, FileStamp::from_metadata(&metadata)) {
        ReadState::Current => None,
        ReadState::Never => Some(ToolOutput::failure(
            "unread_file",
            "this memory already exists and has not been loaded in this \
             session; load_memory it first, then write the version that \
             carries forward what still holds",
        )),
        ReadState::Stale => Some(ToolOutput::failure(
            "stale_read",
            "this memory changed on disk after it was loaded, so writing it \
             now would discard those changes; load it again first",
        )),
        ReadState::Evicted => Some(ToolOutput::failure(
            "evicted_read",
            "this memory was loaded, but that reading was since compacted out \
             of the conversation; load_memory it again before replacing it",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::frontmatter;
    use crate::tool::load_memory::LoadMemory;
    use crate::tool::{Tool, execute_for_test};

    fn unique_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kuncode-write-memory-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn valid_args() -> serde_json::Value {
        serde_json::json!({
            "name": "api-conventions",
            "description": "When touching the API.",
            "type": "project",
            "body": "Use snake_case endpoints.",
        })
    }

    #[tokio::test]
    async fn creating_a_new_memory_writes_frontmatter_and_body() {
        let root = unique_root("create");
        let tool = Arc::new(WriteMemory::new(root.clone()));

        let output = execute_for_test(tool, valid_args(), &ToolContext::new())
            .await
            .expect("no harness error");

        assert!(output.ok);
        let data = output.data.expect("data present");
        assert_eq!(data["created"], true);
        let stored =
            fs::read_to_string(document_path(&root, "api-conventions")).expect("document stored");
        let _ = fs::remove_dir_all(&root);
        let parsed = frontmatter::parse(&stored);
        assert_eq!(parsed.field("name"), Some("api-conventions"));
        assert_eq!(parsed.field("description"), Some("When touching the API."));
        assert_eq!(parsed.field("type"), Some("project"));
        assert_eq!(parsed.body(), "Use snake_case endpoints.\n");
    }

    #[tokio::test]
    async fn overwriting_an_unseen_memory_is_refused_before_approval() {
        let root = unique_root("blind");
        fs::create_dir_all(&root).expect("root");
        fs::write(document_path(&root, "api-conventions"), "original").expect("write");
        let tool = Arc::new(WriteMemory::new(root.clone()));

        // Preparation itself refuses, so no approval decision is ever spent.
        let prepared = tool.prepare(valid_args(), &PreparationContext::new()).await;
        let output = prepared.err().expect("preparation should refuse");
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "unread_file"
        );
        // The refusal is only worth anything if the file survived it.
        assert_eq!(
            fs::read_to_string(document_path(&root, "api-conventions")).expect("readable"),
            "original"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_load_licenses_the_overwrite() {
        let root = unique_root("licensed");
        fs::create_dir_all(&root).expect("root");
        fs::write(document_path(&root, "api-conventions"), "original").expect("write");
        let ctx = ToolContext::new();

        let loaded = execute_for_test(
            Arc::new(LoadMemory::new(root.clone())),
            serde_json::json!({ "name": "api-conventions" }),
            &ctx,
        )
        .await
        .expect("no harness error");
        assert!(loaded.ok);

        let output = execute_for_test(Arc::new(WriteMemory::new(root.clone())), valid_args(), &ctx)
            .await
            .expect("no harness error");

        assert!(output.ok, "{:?}", output.error);
        assert_eq!(output.data.expect("data present")["created"], false);
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn writing_the_same_memory_twice_is_allowed() {
        let root = unique_root("twice");
        let ctx = ToolContext::new();
        let tool = Arc::new(WriteMemory::new(root.clone()));

        let first = execute_for_test(Arc::clone(&tool), valid_args(), &ctx)
            .await
            .expect("no harness error");
        assert!(first.ok);
        // The write itself recorded the reading, so a second write needs no load.
        let second = execute_for_test(tool, valid_args(), &ctx)
            .await
            .expect("no harness error");

        assert!(second.ok, "{:?}", second.error);
        assert_eq!(second.data.expect("data present")["created"], false);
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_change_after_the_load_is_refused_as_stale() {
        let root = unique_root("stale");
        fs::create_dir_all(&root).expect("root");
        fs::write(document_path(&root, "api-conventions"), "original").expect("write");
        let ctx = ToolContext::new();

        let loaded = execute_for_test(
            Arc::new(LoadMemory::new(root.clone())),
            serde_json::json!({ "name": "api-conventions" }),
            &ctx,
        )
        .await
        .expect("no harness error");
        assert!(loaded.ok);
        // A different size guarantees the stamp disagrees with the recording.
        fs::write(
            document_path(&root, "api-conventions"),
            "changed behind the session's back",
        )
        .expect("write");

        let output = execute_for_test(Arc::new(WriteMemory::new(root.clone())), valid_args(), &ctx)
            .await
            .expect("no harness error");

        assert!(!output.ok);
        assert_eq!(
            output.error.expect("error present").kind.as_str(),
            "stale_read"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn the_permission_check_is_an_edit_with_a_preview() {
        let root = unique_root("check");
        fs::create_dir_all(&root).expect("root");
        fs::write(document_path(&root, "api-conventions"), "original").expect("write");
        let ctx = ToolContext::new();

        let loaded = execute_for_test(
            Arc::new(LoadMemory::new(root.clone())),
            serde_json::json!({ "name": "api-conventions" }),
            &ctx,
        )
        .await
        .expect("no harness error");
        assert!(loaded.ok);

        let preparation = Arc::new(WriteMemory::new(root.clone()))
            .prepare(valid_args(), &ctx.preparation())
            .await
            .expect("valid preparation");
        let _ = fs::remove_dir_all(&root);

        match preparation.checks().first().target() {
            PermissionTarget::Edit(path) => {
                assert!(
                    path.as_str().ends_with("api-conventions.md"),
                    "{}",
                    path.as_str()
                );
            }
            other => panic!("expected an Edit target, got {other:?}"),
        }
        assert_eq!(
            preparation.display().summary(),
            "Write memory: api-conventions"
        );
        // Replacing existing content is exactly what the preview must show.
        assert!(preparation.display().preview().is_some());
    }

    #[tokio::test]
    async fn canonical_input_is_rebuilt_with_the_normalized_slug() {
        let root = unique_root("canonical");
        let preparation = Arc::new(WriteMemory::new(root.clone()))
            .prepare(
                serde_json::json!({
                    "name": "  api-conventions  ",
                    "description": "d",
                    "body": "b",
                }),
                &PreparationContext::new(),
            )
            .await
            .expect("valid preparation");
        let _ = fs::remove_dir_all(&root);

        let canonical = preparation.canonical_input().as_value();
        assert_eq!(canonical["name"], "api-conventions");
        // An omitted kind stays omitted, so it cannot fork the fingerprint.
        assert!(canonical.get("type").is_none());
    }

    #[tokio::test]
    async fn blank_fields_and_invalid_kinds_are_rejected() {
        let root = unique_root("invalid");
        let tool = Arc::new(WriteMemory::new(root.clone()));

        for (args, expected) in [
            (
                serde_json::json!({ "name": "UPPER", "description": "d", "body": "b" }),
                "invalid_arguments",
            ),
            (
                serde_json::json!({ "name": "ok", "description": "  ", "body": "b" }),
                "invalid_arguments",
            ),
            (
                serde_json::json!({ "name": "ok", "description": "d", "body": "" }),
                "invalid_arguments",
            ),
            // An unknown kind dies in schema-driven argument parsing.
            (
                serde_json::json!({ "name": "ok", "description": "d", "body": "b", "type": "banana" }),
                "invalid_arguments",
            ),
        ] {
            let output = execute_for_test(Arc::clone(&tool), args.clone(), &ToolContext::new())
                .await
                .expect("no harness error");
            assert!(!output.ok, "{args}");
            assert_eq!(
                output.error.expect("error present").kind.as_str(),
                expected,
                "{args}"
            );
        }
        // Nothing was ever written, not even the directory.
        assert!(!root.exists());
    }
}
