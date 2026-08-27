//! Discovery of the project instruction documents folded into the system prompt.
//!
//! Loading is deliberately non-fatal in every failure mode: a missing,
//! unreadable, or non-UTF-8 document degrades to "no instructions from here"
//! rather than blocking a run that would otherwise work.

use std::path::{Path, PathBuf};

use kuncode_agent::system_prompt::InstructionDocument;

/// The one recognized file name. A single name keeps precedence unambiguous
/// and matches the cross-agent `AGENTS.md` convention, so a repository writes
/// its rules once rather than per tool.
const DOCUMENT_NAME: &str = "AGENTS.md";

/// Home-relative directory holding the user-global document, matching the
/// project-level `.kuncode/` convention.
const GLOBAL_DIR: &str = ".kuncode";

/// Bytes taken from one document. A document is a prompt prefix paid for on
/// every request, so an oversized one is truncated (with the cut announced in
/// the text) instead of silently dominating the context budget.
const MAX_DOCUMENT_BYTES: usize = 64 * 1024;

/// Collects the instruction documents for a run, ordered least to most
/// specific: the user-global document first, then the workspace's own.
///
/// `home` is optional because a missing home directory is a survivable
/// environment, not an error.
pub(crate) fn load_instructions(root: &Path, home: Option<&Path>) -> Vec<InstructionDocument> {
    let mut found: Vec<(PathBuf, String)> = Vec::new();

    if let Some(home) = home {
        found.extend(read_document(&home.join(GLOBAL_DIR)));
    }
    if let Some((path, body)) = read_document(root) {
        // Running kuncode inside the global directory itself would otherwise
        // load the same file twice.
        if found.iter().all(|(seen, _)| seen != &path) {
            found.push((path, body));
        }
    }

    for (path, _) in &found {
        tracing::debug!(
            target: "kuncode::runtime",
            source = %path.display(),
            "instruction document loaded",
        );
    }

    found
        .into_iter()
        .map(|(path, body)| InstructionDocument::new(path.display().to_string(), body))
        .collect()
}

/// Reads `dir/AGENTS.md`, or `None` when it is absent, unusable, or blank.
fn read_document(dir: &Path) -> Option<(PathBuf, String)> {
    let path = dir.join(DOCUMENT_NAME);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(
                target: "kuncode::runtime",
                source = %path.display(),
                diagnostic_chars = error.to_string().chars().count(),
                "instruction document could not be read; skipping it",
            );
            return None;
        }
    };
    let Ok(body) = String::from_utf8(raw) else {
        tracing::warn!(
            target: "kuncode::runtime",
            source = %path.display(),
            "instruction document is not valid UTF-8; skipping it",
        );
        return None;
    };
    if body.trim().is_empty() {
        return None;
    }
    let body = truncated(body, &path);
    Some((path, body))
}

/// Caps one document at [`MAX_DOCUMENT_BYTES`], cutting on a char boundary and
/// stating the cut so the model does not read a half sentence as a whole rule.
fn truncated(body: String, path: &Path) -> String {
    if body.len() <= MAX_DOCUMENT_BYTES {
        return body;
    }
    let mut end = MAX_DOCUMENT_BYTES;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    tracing::warn!(
        target: "kuncode::runtime",
        source = %path.display(),
        bytes = body.len(),
        kept_bytes = end,
        "instruction document exceeds the prompt budget and was truncated",
    );
    let total = body.len();
    let mut kept = body;
    kept.truncate(end);
    kept.push_str(&format!(
        "\n\n[truncated: {end} of {total} bytes of this document are shown]"
    ));
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Tests share a process, so the tag keeps concurrent cases apart.
    fn unique_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kuncode-instructions-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn rendered(documents: &[InstructionDocument]) -> String {
        use kuncode_agent::system_prompt::{InstructionsSection, PromptContext, PromptSection};

        InstructionsSection::new(documents.to_vec())
            .render(&PromptContext { tools: &[] })
            .unwrap_or_default()
    }

    #[test]
    fn absent_documents_load_nothing() {
        let dir = unique_dir("absent");
        let documents = load_instructions(&dir, Some(&dir));
        let _ = fs::remove_dir_all(&dir);

        assert!(documents.is_empty());
    }

    #[test]
    fn a_missing_home_is_not_an_error() {
        let dir = unique_dir("no-home");
        fs::write(dir.join(DOCUMENT_NAME), "project rule").expect("write");
        let documents = load_instructions(&dir, None);
        let _ = fs::remove_dir_all(&dir);

        assert!(rendered(&documents).contains("project rule"));
    }

    #[test]
    fn other_agent_conventions_are_not_loaded() {
        let dir = unique_dir("other-names");
        fs::write(dir.join("KUNCODE.md"), "kuncode rule").expect("write");
        fs::write(dir.join("CLAUDE.md"), "claude rule").expect("write");
        let documents = load_instructions(&dir, None);
        let _ = fs::remove_dir_all(&dir);

        assert!(documents.is_empty());
    }

    #[test]
    fn a_blank_document_loads_nothing() {
        let dir = unique_dir("blank");
        fs::write(dir.join(DOCUMENT_NAME), "   \n\n").expect("write");
        let documents = load_instructions(&dir, None);
        let _ = fs::remove_dir_all(&dir);

        assert!(documents.is_empty());
    }

    #[test]
    fn the_global_document_renders_before_the_project_one() {
        let home = unique_dir("home");
        let project = unique_dir("project");
        fs::create_dir_all(home.join(GLOBAL_DIR)).expect("global dir");
        fs::write(home.join(GLOBAL_DIR).join(DOCUMENT_NAME), "global rule").expect("write");
        fs::write(project.join(DOCUMENT_NAME), "project rule").expect("write");

        let documents = load_instructions(&project, Some(&home));
        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&project);

        assert_eq!(documents.len(), 2);
        let block = rendered(&documents);
        let global = block.find("global rule").expect("global renders");
        let project = block.find("project rule").expect("project renders");
        assert!(global < project, "{block}");
    }

    #[test]
    fn the_same_file_is_not_loaded_twice() {
        let home = unique_dir("same-file-home");
        let global = home.join(GLOBAL_DIR);
        fs::create_dir_all(&global).expect("global dir");
        fs::write(global.join(DOCUMENT_NAME), "one rule").expect("write");

        // Running from inside the global directory makes both scans find it.
        let documents = load_instructions(&global, Some(&home));
        let _ = fs::remove_dir_all(&home);

        assert_eq!(documents.len(), 1);
    }

    #[test]
    fn an_oversized_document_is_cut_on_a_char_boundary_and_says_so() {
        let dir = unique_dir("oversized");
        // Multi-byte chars straddle the cap, so a naive byte cut would panic.
        let body = "。".repeat(MAX_DOCUMENT_BYTES);
        fs::write(dir.join(DOCUMENT_NAME), &body).expect("write");
        let documents = load_instructions(&dir, None);
        let _ = fs::remove_dir_all(&dir);

        let block = rendered(&documents);
        assert!(block.contains("[truncated:"), "{block}");
        assert!(block.len() < body.len(), "the document was not truncated");
    }

    #[test]
    fn a_non_utf8_document_is_skipped() {
        let dir = unique_dir("binary");
        fs::write(dir.join(DOCUMENT_NAME), [0xff, 0xfe, 0x00]).expect("write");
        let documents = load_instructions(&dir, None);
        let _ = fs::remove_dir_all(&dir);

        assert!(documents.is_empty());
    }
}
