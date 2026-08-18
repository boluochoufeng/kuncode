//! Skill discovery: the catalog behind on-demand instruction loading.
//!
//! A skill is a directory holding a `SKILL.md` document — reusable instructions
//! for one kind of task. The catalog is scanned once at startup; only each
//! skill's name and one-line description enter the system prompt, while the
//! full document stays on disk until the model asks for it through the
//! `load_skill` tool. This keeps per-request prompt cost proportional to the
//! catalog, not to the sum of every document.
//!
//! Like the `AGENTS.md` instruction documents, skill bodies are
//! workspace-controlled text: opening a project is the trust decision that
//! admits them, and the permission engine stays the boundary on what the model
//! may then do.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The one recognized document name inside a skill directory.
const DOCUMENT_NAME: &str = "SKILL.md";

/// Longest accepted skill name, in characters. Names are identifiers that the
/// model echoes back; anything longer is a misconfigured directory.
const MAX_NAME_CHARS: usize = 64;

/// Longest catalog description, in characters. The description is prompt
/// prefix paid on every request, so it is flattened and capped rather than
/// trusted to stay short.
const MAX_DESCRIPTION_CHARS: usize = 256;

/// Bytes of one skill document handed to the model per load. Mirrors the
/// instruction-document budget: an oversized body is cut on a char boundary
/// with the cut announced, instead of dominating the context.
pub const MAX_SKILL_BYTES: usize = 64 * 1024;

/// Catalog line for one discovered skill; the body stays on disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillSummary {
    name: String,
    description: String,
}

impl SkillSummary {
    /// Builds a summary directly, applying the same flattening and caps as a
    /// disk scan so the one-line catalog invariant holds for every producer.
    pub fn new(name: impl AsRef<str>, description: impl AsRef<str>) -> Self {
        Self {
            name: flatten(name.as_ref(), MAX_NAME_CHARS),
            description: flatten(description.as_ref(), MAX_DESCRIPTION_CHARS),
        }
    }

    /// The registry key the model passes to `load_skill`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// One flattened line describing when the skill applies.
    pub fn description(&self) -> &str {
        &self.description
    }
}

/// One discovered skill: catalog metadata plus the document it points at.
#[derive(Clone, Debug)]
struct Skill {
    summary: SkillSummary,
    path: PathBuf,
}

/// Immutable, name-keyed registry of the skills discovered at startup.
///
/// Lookup is by registered name only — a requested name is never treated as a
/// path, so the model cannot steer `load_skill` outside the scanned roots.
#[derive(Clone, Debug, Default)]
pub struct SkillCatalog {
    /// Name-sorted so the rendered catalog is deterministic regardless of
    /// directory iteration order — it is part of the cached prompt prefix.
    skills: BTreeMap<String, Skill>,
}

impl SkillCatalog {
    /// Scans `roots` in order for `{root}/{name}/SKILL.md` documents.
    ///
    /// A later root overrides an earlier one on a name collision, so callers
    /// pass roots least to most specific (user-global before workspace).
    /// Every failure mode — missing root, unreadable or non-UTF-8 document,
    /// blank content — degrades to "no skill from here" with a log line,
    /// never to a startup error.
    pub fn scan(roots: &[PathBuf]) -> Self {
        let mut skills = BTreeMap::new();
        for root in roots {
            for skill in scan_root(root) {
                if let Some(previous) = skills.insert(skill.summary.name.clone(), skill) {
                    tracing::debug!(
                        target: "kuncode::skill",
                        name = %previous.summary.name,
                        overridden = %previous.path.display(),
                        "workspace skill overrides the global one",
                    );
                }
            }
        }
        Self { skills }
    }

    /// Returns `true` when no skills were discovered.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Number of discovered skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Catalog lines in stable name order, for the system-prompt section.
    pub fn summaries(&self) -> Vec<SkillSummary> {
        self.skills
            .values()
            .map(|skill| skill.summary.clone())
            .collect()
    }

    /// Resolves a registered name to the document path it was scanned from.
    pub fn document_path(&self, name: &str) -> Option<&Path> {
        self.skills.get(name).map(|skill| skill.path.as_path())
    }
}

/// Collects the valid skills directly under one root, in deterministic order.
fn scan_root(root: &Path) -> Vec<Skill> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(
                target: "kuncode::skill",
                root = %root.display(),
                diagnostic_chars = error.to_string().chars().count(),
                "skills root could not be read; skipping it",
            );
            return Vec::new();
        }
    };
    let mut directories: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    // read_dir order is filesystem-dependent; within one root the order only
    // decides log order (the catalog re-sorts by name), but determinism is
    // cheap and keeps scans reproducible.
    directories.sort();
    directories
        .into_iter()
        .filter_map(|dir| read_skill(&dir))
        .collect()
}

/// Reads one skill directory, or `None` when it holds no usable document.
fn read_skill(dir: &Path) -> Option<Skill> {
    let path = dir.join(DOCUMENT_NAME);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(
                target: "kuncode::skill",
                source = %path.display(),
                diagnostic_chars = error.to_string().chars().count(),
                "skill document could not be read; skipping it",
            );
            return None;
        }
    };
    let Ok(body) = String::from_utf8(raw) else {
        tracing::warn!(
            target: "kuncode::skill",
            source = %path.display(),
            "skill document is not valid UTF-8; skipping it",
        );
        return None;
    };
    if body.trim().is_empty() {
        return None;
    }

    let parsed = parse_document(&body);
    // The directory name is the stable identity; frontmatter may override it
    // but an absent or blank field must not orphan the skill.
    let fallback_name = dir.file_name()?.to_str()?;
    let name = flatten(parsed.name.unwrap_or(fallback_name), MAX_NAME_CHARS);
    if name.is_empty() {
        return None;
    }
    let description = flatten(
        parsed
            .description
            .or_else(|| parsed.body.lines().find(|line| !line.trim().is_empty()))
            .unwrap_or(""),
        MAX_DESCRIPTION_CHARS,
    );
    tracing::debug!(
        target: "kuncode::skill",
        name = %name,
        source = %path.display(),
        "skill discovered",
    );
    Some(Skill {
        summary: SkillSummary::new(name, description),
        path,
    })
}

/// Frontmatter fields plus the body that follows them.
struct ParsedDocument<'a> {
    name: Option<&'a str>,
    description: Option<&'a str>,
    body: &'a str,
}

/// Splits optional `---`-delimited frontmatter from the body.
///
/// Only the two `key: value` fields the catalog uses are recognized; unknown
/// keys are ignored rather than rejected, so a skill written for a richer
/// harness still loads here. Hand-rolled because this subset needs no YAML
/// dependency: values are single-line scalars.
fn parse_document(document: &str) -> ParsedDocument<'_> {
    let plain = ParsedDocument {
        name: None,
        description: None,
        body: document,
    };
    let Some(after_open) = document
        .strip_prefix("---\n")
        .or_else(|| document.strip_prefix("---\r\n"))
    else {
        return plain;
    };
    let mut name = None;
    let mut description = None;
    let mut cursor = 0usize;
    while cursor < after_open.len() {
        let line_end = after_open[cursor..].find('\n').map(|found| cursor + found);
        let line = match line_end {
            Some(end) => &after_open[cursor..end],
            None => &after_open[cursor..],
        };
        let line = line.trim_end_matches('\r');
        if line.trim() == "---" {
            let body = match line_end {
                Some(end) => &after_open[end + 1..],
                None => "",
            };
            return ParsedDocument {
                name,
                description,
                body,
            };
        }
        if let Some((key, value)) = line.split_once(':') {
            match key.trim() {
                "name" => name = Some(value.trim()),
                "description" => description = Some(value.trim()),
                _ => {}
            }
        }
        match line_end {
            Some(end) => cursor = end + 1,
            None => break,
        }
    }
    // An unterminated frontmatter block is treated as plain body: guessing
    // where it "would" end risks hiding most of the document.
    plain
}

/// Collapses whitespace runs to single spaces and caps the char count, so one
/// catalog entry stays one line of bounded prompt cost.
fn flatten(text: &str, max_chars: usize) -> String {
    let mut flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max_chars {
        flat = flat.chars().take(max_chars).collect();
    }
    flat
}

/// Caps a loaded body at [`MAX_SKILL_BYTES`], cutting on a char boundary and
/// stating the cut. Returns the text and whether anything was dropped.
pub(crate) fn bounded_body(body: String) -> (String, bool) {
    if body.len() <= MAX_SKILL_BYTES {
        return (body, false);
    }
    let mut end = MAX_SKILL_BYTES;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    let total = body.len();
    let mut kept = body;
    kept.truncate(end);
    kept.push_str(&format!(
        "\n\n[truncated: {end} of {total} bytes of this skill are shown]"
    ));
    (kept, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Tests share a process, so the tag keeps concurrent cases apart.
    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kuncode-skill-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_skill(root: &Path, dir_name: &str, content: &str) {
        let dir = root.join(dir_name);
        fs::create_dir_all(&dir).expect("skill dir");
        fs::write(dir.join(DOCUMENT_NAME), content).expect("skill document");
    }

    #[test]
    fn frontmatter_fields_win_over_fallbacks() {
        let root = unique_dir("frontmatter");
        write_skill(
            &root,
            "dir-name",
            "---\nname: pdf\ndescription: Process PDF files.\nextra: ignored\n---\nBody here.",
        );

        let catalog = SkillCatalog::scan(std::slice::from_ref(&root));
        let _ = fs::remove_dir_all(&root);

        let summaries = catalog.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name(), "pdf");
        assert_eq!(summaries[0].description(), "Process PDF files.");
        assert!(catalog.document_path("pdf").is_some());
        // Lookup is by registered name, never the directory name it replaced.
        assert!(catalog.document_path("dir-name").is_none());
    }

    #[test]
    fn missing_frontmatter_falls_back_to_directory_and_first_line() {
        let root = unique_dir("fallback");
        write_skill(
            &root,
            "code-review",
            "Review changed code carefully.\nMore.",
        );

        let catalog = SkillCatalog::scan(std::slice::from_ref(&root));
        let _ = fs::remove_dir_all(&root);

        let summaries = catalog.summaries();
        assert_eq!(summaries[0].name(), "code-review");
        assert_eq!(summaries[0].description(), "Review changed code carefully.");
    }

    #[test]
    fn later_roots_override_earlier_ones_by_name() {
        let global = unique_dir("override-global");
        let project = unique_dir("override-project");
        write_skill(&global, "deploy", "global instructions");
        write_skill(&project, "deploy", "project instructions");

        let catalog = SkillCatalog::scan(&[global.clone(), project.clone()]);
        let path = catalog
            .document_path("deploy")
            .expect("skill resolves")
            .to_path_buf();
        let _ = fs::remove_dir_all(&global);
        let _ = fs::remove_dir_all(&project);

        assert_eq!(catalog.len(), 1);
        assert!(path.starts_with(&project), "resolved {}", path.display());
    }

    #[test]
    fn summaries_are_name_sorted_for_a_stable_prompt_prefix() {
        let root = unique_dir("sorted");
        write_skill(&root, "zeta", "z");
        write_skill(&root, "alpha", "a");
        write_skill(&root, "mid", "m");

        let catalog = SkillCatalog::scan(std::slice::from_ref(&root));
        let _ = fs::remove_dir_all(&root);

        let names: Vec<String> = catalog
            .summaries()
            .iter()
            .map(|summary| summary.name().to_string())
            .collect();
        assert_eq!(names, ["alpha", "mid", "zeta"]);
    }

    #[test]
    fn unusable_directories_are_skipped_not_fatal() {
        let root = unique_dir("unusable");
        // A directory without SKILL.md, a blank document, and a non-UTF-8 one.
        fs::create_dir_all(root.join("empty-dir")).expect("dir");
        write_skill(&root, "blank", "   \n");
        let binary = root.join("binary");
        fs::create_dir_all(&binary).expect("dir");
        fs::write(binary.join(DOCUMENT_NAME), [0xff, 0xfe, 0x00]).expect("write");
        // A stray file directly under the root is not a skill directory.
        fs::write(root.join("stray.md"), "not a skill").expect("write");
        write_skill(&root, "good", "usable");

        let catalog = SkillCatalog::scan(std::slice::from_ref(&root));
        let _ = fs::remove_dir_all(&root);

        assert_eq!(catalog.len(), 1);
        assert!(catalog.document_path("good").is_some());
    }

    #[test]
    fn a_missing_root_scans_to_an_empty_catalog() {
        let root = unique_dir("missing-root").join("does-not-exist");

        let catalog = SkillCatalog::scan(&[root]);

        assert!(catalog.is_empty());
    }

    #[test]
    fn descriptions_are_flattened_and_capped() {
        let root = unique_dir("flatten");
        write_skill(
            &root,
            "wordy",
            &format!(
                "---\nname: wordy\ndescription: spread\tover   {}\n---\nbody",
                "x".repeat(400)
            ),
        );

        let catalog = SkillCatalog::scan(std::slice::from_ref(&root));
        let _ = fs::remove_dir_all(&root);

        let summaries = catalog.summaries();
        let description = summaries[0].description();
        assert!(description.starts_with("spread over x"));
        assert_eq!(description.chars().count(), MAX_DESCRIPTION_CHARS);
    }

    #[test]
    fn unterminated_frontmatter_is_treated_as_body() {
        let root = unique_dir("unterminated");
        write_skill(&root, "broken", "---\nname: hidden\nno closing delimiter");

        let catalog = SkillCatalog::scan(std::slice::from_ref(&root));
        let _ = fs::remove_dir_all(&root);

        let summaries = catalog.summaries();
        // The directory name stays authoritative; the pseudo-frontmatter line
        // becomes the description like any other first body line.
        assert_eq!(summaries[0].name(), "broken");
        assert_eq!(summaries[0].description(), "---");
    }

    #[test]
    fn oversized_bodies_are_cut_on_a_char_boundary_and_say_so() {
        let body = "。".repeat(MAX_SKILL_BYTES);

        let (kept, truncated) = bounded_body(body.clone());

        assert!(truncated);
        assert!(kept.len() < body.len());
        assert!(kept.contains("[truncated:"), "{kept}");

        let (same, untruncated) = bounded_body("small".to_string());
        assert!(!untruncated);
        assert_eq!(same, "small");
    }
}
