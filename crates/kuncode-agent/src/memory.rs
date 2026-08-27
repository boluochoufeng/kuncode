//! Cross-session memory: the storage contract behind explicit recall.
//!
//! A memory is one flat `{root}/{slug}.md` document under the per-project
//! memory root (outside the workspace, so writes never dirty the repository).
//! Like skills, the catalog is scanned once at startup and only names plus
//! one-line descriptions enter the system prompt; the full document stays on
//! disk until the model asks through `load_memory`. Unlike skills, documents
//! are written by the model itself through `write_memory`, so the slug
//! grammar doubles as the path-confinement boundary: every name a tool
//! accepts resolves lexically to a file directly under the root.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::frontmatter::{self, flatten};
use crate::session_store::project_slug;

/// Longest accepted memory name (slug), in characters.
pub const MAX_SLUG_CHARS: usize = 64;

/// Longest catalog description, in characters. Prompt prefix paid on every
/// request, so it is flattened and capped like a skill description.
const MAX_DESCRIPTION_CHARS: usize = 256;

/// The recognized kinds of memory, serialized lowercase into the frontmatter
/// `type` field. The kind guides what the model stores; the harness only
/// round-trips it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    /// Durable facts about the user (role, expertise, preferences).
    User,
    /// Guidance the user gave on how to work, worth not re-learning.
    Feedback,
    /// Project facts and decisions not derivable from the repository.
    Project,
    /// Pointers to external resources.
    Reference,
}

impl MemoryKind {
    /// Frontmatter value, identical to the serde form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }
}

/// Why a model-supplied memory name is not a valid slug.
#[derive(Debug, Error)]
pub enum MemorySlugError {
    #[error("memory name must not be empty")]
    Empty,
    #[error("memory name must be at most {MAX_SLUG_CHARS} characters")]
    TooLong,
    #[error("memory name may only contain lowercase letters, digits and hyphens")]
    InvalidCharacter,
}

/// Validates a name into the slug that names its file.
///
/// The grammar (`[a-z0-9-]+`, at most [`MAX_SLUG_CHARS`]) is also what makes
/// path traversal impossible: no separators, no dots, no way to name anything
/// outside the memory root.
///
/// # Errors
///
/// Returns a [`MemorySlugError`] naming the first violated rule.
pub fn validate_slug(name: &str) -> Result<String, MemorySlugError> {
    if name.is_empty() {
        return Err(MemorySlugError::Empty);
    }
    if name.chars().count() > MAX_SLUG_CHARS {
        return Err(MemorySlugError::TooLong);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(MemorySlugError::InvalidCharacter);
    }
    Ok(name.to_string())
}

/// Returns the per-project memory root: `{home}/.kuncode/memory/{project}`.
///
/// Keyed by [`project_slug`] so memories are isolated per project while the
/// files stay outside the workspace tree.
pub fn memory_root(home: &Path, project_root: &Path) -> PathBuf {
    home.join(".kuncode")
        .join("memory")
        .join(project_slug(project_root))
}

/// The file a slug denotes: `{root}/{slug}.md`.
///
/// Purely lexical, so a memory written earlier this session resolves without
/// the startup catalog.
pub fn document_path(root: &Path, slug: &str) -> PathBuf {
    root.join(format!("{slug}.md"))
}

/// Slugs currently stored under `root`, sorted.
///
/// Read live rather than from the startup catalog so not-found diagnostics
/// include memories written this session.
pub(crate) fn known_slugs(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut slugs: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .filter_map(|path| path.file_stem()?.to_str().map(str::to_string))
        .filter(|stem| validate_slug(stem).is_ok())
        .collect();
    slugs.sort();
    slugs
}

/// Renders the document `write_memory` stores; the dual of
/// [`frontmatter::parse`].
///
/// The description is flattened to one line first and the slug grammar
/// excludes `:` and newlines, so parsing always recovers both fields. A `---`
/// line inside the body is harmless: parsing already closed the block at the
/// first delimiter.
pub(crate) fn render_document(
    slug: &str,
    description: &str,
    kind: Option<MemoryKind>,
    body: &str,
) -> String {
    let description = flatten(description, MAX_DESCRIPTION_CHARS);
    let mut document = format!("---\nname: {slug}\ndescription: {description}\n");
    if let Some(kind) = kind {
        document.push_str("type: ");
        document.push_str(kind.as_str());
        document.push('\n');
    }
    document.push_str("---\n");
    document.push_str(body);
    if !body.ends_with('\n') {
        document.push('\n');
    }
    document
}

/// Index line for one stored memory; the body stays on disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemorySummary {
    name: String,
    description: String,
}

impl MemorySummary {
    /// Builds a summary, applying the same flattening and caps as a disk scan
    /// so the one-line index invariant holds for every producer.
    pub fn new(name: impl AsRef<str>, description: impl AsRef<str>) -> Self {
        Self {
            name: flatten(name.as_ref(), MAX_SLUG_CHARS),
            description: flatten(description.as_ref(), MAX_DESCRIPTION_CHARS),
        }
    }

    /// The name the model passes to `load_memory`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// One flattened line stating when the memory applies.
    pub fn description(&self) -> &str {
        &self.description
    }
}

/// Startup-scanned index of stored memories, name-sorted for a stable prompt
/// prefix.
///
/// The catalog only feeds the system-prompt index: `load_memory` resolves
/// paths lexically from the slug, so a memory written mid-session is loadable
/// immediately and enters the index on the next start — the same
/// startup-frozen contract skills follow.
#[derive(Clone, Debug, Default)]
pub struct MemoryCatalog {
    memories: BTreeMap<String, MemorySummary>,
}

impl MemoryCatalog {
    /// Scans the flat `{root}/{slug}.md` documents.
    ///
    /// The file stem is the identity — the opposite of skills, where
    /// frontmatter may rename — because the index must only list names
    /// `load_memory` can actually resolve. A frontmatter `name` that
    /// disagrees is ignored with a log line. Every failure mode — missing
    /// root, invalid stem, unreadable or non-UTF-8 or blank document —
    /// degrades to "no memory from here", never to a startup error.
    pub fn scan(root: &Path) -> Self {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self::default();
            }
            Err(error) => {
                tracing::warn!(
                    target: "kuncode::memory",
                    root = %root.display(),
                    diagnostic_chars = error.to_string().chars().count(),
                    "memory root could not be read; skipping it",
                );
                return Self::default();
            }
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        // read_dir order is filesystem-dependent; the catalog re-sorts by
        // name, but a deterministic scan keeps log order reproducible.
        files.sort();
        let memories = files
            .into_iter()
            .filter_map(|path| {
                let summary = read_memory(&path)?;
                Some((summary.name.clone(), summary))
            })
            .collect();
        Self { memories }
    }

    /// Returns `true` when no memories are stored.
    pub fn is_empty(&self) -> bool {
        self.memories.is_empty()
    }

    /// Number of stored memories.
    pub fn len(&self) -> usize {
        self.memories.len()
    }

    /// Index lines in stable name order, for the system-prompt section.
    pub fn summaries(&self) -> Vec<MemorySummary> {
        self.memories.values().cloned().collect()
    }
}

/// Reads one memory document, or `None` when it is not usable.
fn read_memory(path: &Path) -> Option<MemorySummary> {
    let stem = path.file_stem()?.to_str()?;
    if validate_slug(stem).is_err() {
        tracing::debug!(
            target: "kuncode::memory",
            source = %path.display(),
            "file stem is not a valid memory name; skipping it",
        );
        return None;
    }
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) => {
            tracing::warn!(
                target: "kuncode::memory",
                source = %path.display(),
                diagnostic_chars = error.to_string().chars().count(),
                "memory document could not be read; skipping it",
            );
            return None;
        }
    };
    let Ok(body) = String::from_utf8(raw) else {
        tracing::warn!(
            target: "kuncode::memory",
            source = %path.display(),
            "memory document is not valid UTF-8; skipping it",
        );
        return None;
    };
    if body.trim().is_empty() {
        return None;
    }

    let parsed = frontmatter::parse(&body);
    if parsed.field("name").is_some_and(|name| name != stem) {
        tracing::debug!(
            target: "kuncode::memory",
            source = %path.display(),
            "frontmatter name disagrees with the file stem; the stem is authoritative",
        );
    }
    let description = parsed
        .field("description")
        .or_else(|| parsed.body().lines().find(|line| !line.trim().is_empty()))
        .unwrap_or("");
    Some(MemorySummary::new(stem, description))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Tests share a process, so the tag keeps concurrent cases apart.
    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kuncode-memory-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_memory_file(root: &Path, slug: &str, content: &str) {
        fs::write(document_path(root, slug), content).expect("memory document");
    }

    #[test]
    fn scan_lists_documents_sorted_by_stem() {
        let root = unique_dir("sorted");
        write_memory_file(&root, "zeta", "z fact");
        write_memory_file(&root, "alpha", "---\ndescription: a fact\n---\nbody");
        write_memory_file(&root, "mid", "m fact");

        let catalog = MemoryCatalog::scan(&root);
        let _ = fs::remove_dir_all(&root);

        let names: Vec<String> = catalog
            .summaries()
            .iter()
            .map(|summary| summary.name().to_string())
            .collect();
        assert_eq!(names, ["alpha", "mid", "zeta"]);
        assert_eq!(catalog.len(), 3);
        assert_eq!(catalog.summaries()[0].description(), "a fact");
        // Missing frontmatter falls back to the first non-blank body line.
        assert_eq!(catalog.summaries()[1].description(), "m fact");
    }

    #[test]
    fn the_file_stem_is_the_identity_over_frontmatter_name() {
        let root = unique_dir("stem-identity");
        write_memory_file(&root, "real-name", "---\nname: impostor\n---\nbody");

        let catalog = MemoryCatalog::scan(&root);
        let _ = fs::remove_dir_all(&root);

        let summaries = catalog.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name(), "real-name");
    }

    #[test]
    fn unusable_files_are_skipped_not_fatal() {
        let root = unique_dir("unusable");
        write_memory_file(&root, "good", "usable");
        fs::write(root.join("Bad-Stem.md"), "uppercase stem").expect("write");
        fs::write(root.join("stray.txt"), "wrong extension").expect("write");
        fs::write(root.join("blank.md"), "   \n").expect("write");
        fs::write(root.join("binary.md"), [0xff, 0xfe, 0x00]).expect("write");
        fs::create_dir_all(root.join("subdir.md")).expect("dir");

        let catalog = MemoryCatalog::scan(&root);
        let _ = fs::remove_dir_all(&root);

        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog.summaries()[0].name(), "good");
    }

    #[test]
    fn a_missing_root_scans_to_an_empty_catalog() {
        let root = unique_dir("missing").join("does-not-exist");

        let catalog = MemoryCatalog::scan(&root);

        assert!(catalog.is_empty());
    }

    #[test]
    fn the_slug_grammar_admits_nothing_that_could_leave_the_root() {
        assert_eq!(
            validate_slug("api-conventions").expect("valid"),
            "api-conventions"
        );
        assert_eq!(validate_slug("a1").expect("valid"), "a1");

        assert!(matches!(validate_slug(""), Err(MemorySlugError::Empty)));
        assert!(matches!(
            validate_slug(&"a".repeat(MAX_SLUG_CHARS + 1)),
            Err(MemorySlugError::TooLong)
        ));
        for invalid in ["../escape", "a/b", "A", "a b", "a_b", "a.md", "记忆"] {
            assert!(
                matches!(
                    validate_slug(invalid),
                    Err(MemorySlugError::InvalidCharacter)
                ),
                "{invalid} should be rejected",
            );
        }
    }

    #[test]
    fn render_document_round_trips_through_frontmatter_parse() {
        let body = "First line.\n---\nA delimiter inside the body.";
        let document = render_document(
            "api-notes",
            "When touching the API.",
            Some(MemoryKind::Project),
            body,
        );

        let parsed = frontmatter::parse(&document);
        assert_eq!(parsed.field("name"), Some("api-notes"));
        assert_eq!(parsed.field("description"), Some("When touching the API."));
        assert_eq!(parsed.field("type"), Some("project"));
        assert_eq!(parsed.body(), format!("{body}\n"));

        // Without a kind the `type` line is omitted entirely.
        let document = render_document("plain", "desc", None, "body\n");
        let parsed = frontmatter::parse(&document);
        assert_eq!(parsed.field("type"), None);
        assert_eq!(parsed.body(), "body\n");
    }

    #[test]
    fn rendered_descriptions_are_flattened_to_one_line() {
        let document = render_document("wordy", "spread\nover\tlines", None, "body");

        let parsed = frontmatter::parse(&document);
        assert_eq!(parsed.field("description"), Some("spread over lines"));
    }

    #[test]
    fn memory_root_is_home_scoped_and_project_keyed() {
        let root = memory_root(Path::new("/home/u"), Path::new("/work/proj"));

        assert_eq!(root, Path::new("/home/u/.kuncode/memory/-work-proj"));
    }

    #[test]
    fn known_slugs_lists_stored_documents_sorted() {
        let root = unique_dir("known");
        write_memory_file(&root, "beta", "b");
        write_memory_file(&root, "alpha", "a");
        fs::write(root.join("Ignored.md"), "bad stem").expect("write");

        let slugs = known_slugs(&root);
        let _ = fs::remove_dir_all(&root);

        assert_eq!(slugs, ["alpha", "beta"]);
        assert!(known_slugs(Path::new("/does/not/exist")).is_empty());
    }
}
