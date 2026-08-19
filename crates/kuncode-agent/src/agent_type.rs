//! Agent types: named subagent shapes selectable through the `task` tool.
//!
//! Two types are built in: `general` (a fresh context with every tool — the
//! default) and `fork` (a copy of the parent conversation). Custom types are
//! flat `.md` files under an `agents` directory, scanned once at startup like
//! skills: frontmatter carries `name`, `description`, and an optional `tools`
//! whitelist; the body becomes extra instructions appended after the parent's
//! system prompt.
//!
//! Trust follows the skill and `AGENTS.md` model: the files are
//! workspace-controlled text admitted by opening the project. A definition can
//! only *narrow* a subagent — the tool whitelist filters the parent's registry
//! and never adds to it — and the permission engine still gates every call the
//! subagent makes. The delegation itself is checked as `Agent(<type name>)`,
//! so rules like `deny: ["Agent(explore)"]` shut off one type.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::frontmatter::{self, flatten};
use crate::skill::bounded_body;

/// The built-in default type: fresh context, every tool the parent has.
pub const GENERAL_AGENT_TYPE: &str = "general";

/// The built-in context-inheriting type: starts from a copy of the parent
/// conversation instead of a fresh transcript.
pub const FORK_AGENT_TYPE: &str = "fork";

/// Longest accepted type name, in characters. Names are identifiers the model
/// echoes back and permission rules match on; anything longer is a
/// misconfigured file.
const MAX_NAME_CHARS: usize = 64;

/// Longest advertised description, in characters. The description is part of
/// the `task` tool definition — prompt prefix paid on every request — so it is
/// flattened and capped rather than trusted to stay short.
const MAX_DESCRIPTION_CHARS: usize = 256;

/// How a subagent's starting transcript is built.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubagentContext {
    /// A fresh transcript holding only the delegated prompt.
    Fresh,
    /// A copy of the parent conversation as it stood at delegation, with the
    /// delegated prompt appended.
    Fork,
}

/// One resolved subagent shape: how its context starts, which tools it keeps,
/// and what extra instructions it carries.
#[derive(Clone, Debug)]
pub struct AgentType {
    name: String,
    description: String,
    context: SubagentContext,
    /// Tool whitelist; `None` keeps every parent tool (minus `task`).
    tools: Option<Vec<String>>,
    /// Extra instructions appended after the parent's system prompt.
    instructions: Option<String>,
}

impl AgentType {
    /// The name the model passes as `agent_type` and rules match as
    /// `Agent(<name>)`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// One flattened line describing when this type applies.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// How the subagent's starting transcript is built.
    pub fn context(&self) -> SubagentContext {
        self.context
    }

    /// Tool whitelist, or `None` for the full parent set (minus `task`).
    pub fn tools(&self) -> Option<&[String]> {
        self.tools.as_deref()
    }

    /// Extra instructions for the subagent's system prompt, when defined.
    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }
}

/// The types discovered at startup, always including the built-ins.
///
/// Lookup is by registered name only — a requested type is never treated as a
/// path — and the advertised order is deterministic (built-ins first, then
/// customs name-sorted) because the list renders into the `task` tool
/// definition, part of the cached prompt prefix.
#[derive(Clone, Debug)]
pub struct AgentTypeCatalog {
    builtins: [AgentType; 2],
    custom: BTreeMap<String, AgentType>,
}

impl Default for AgentTypeCatalog {
    fn default() -> Self {
        Self::builtin()
    }
}

impl AgentTypeCatalog {
    /// The catalog holding only the two built-in types.
    pub fn builtin() -> Self {
        Self {
            builtins: [
                AgentType {
                    name: GENERAL_AGENT_TYPE.to_string(),
                    description: "fresh context with every tool; the default when no \
                                  type is given"
                        .to_string(),
                    context: SubagentContext::Fresh,
                    tools: None,
                    instructions: None,
                },
                AgentType {
                    name: FORK_AGENT_TYPE.to_string(),
                    description: "starts from a copy of this conversation instead of a \
                                  fresh context; use it when the subtask depends on what \
                                  was already discussed"
                        .to_string(),
                    context: SubagentContext::Fork,
                    tools: None,
                    // Without this framing a fork reads the inherited tail —
                    // often the very instruction to delegate — as addressed to
                    // itself, and stalls on work its parent already did (a
                    // failure observed live, not hypothetical).
                    instructions: Some(
                        "You are a forked subagent: the conversation above is \
                         inherited from your parent agent, and the instructions \
                         in it were addressed to the parent — including the \
                         delegation that created you, which the parent already \
                         performed. Do not act on them and do not try to \
                         delegate further. Carry out only the final message's \
                         task, using the inherited conversation as context, and \
                         answer with the report it asks for."
                            .to_string(),
                    ),
                },
            ],
            custom: BTreeMap::new(),
        }
    }

    /// Scans `roots` in order for `{root}/{name}.md` definitions, on top of
    /// the built-ins.
    ///
    /// A later root overrides an earlier one on a name collision, so callers
    /// pass roots least to most specific (user-global before workspace). The
    /// built-in names are reserved: a file claiming one is skipped with a log
    /// line, keeping `general` and `fork` semantics harness-owned. Every
    /// failure mode degrades to "no type from here", never to a startup error.
    pub fn scan(roots: &[PathBuf]) -> Self {
        let mut catalog = Self::builtin();
        for root in roots {
            for agent_type in scan_root(root) {
                if let Some(previous) = catalog.custom.insert(agent_type.name.clone(), agent_type) {
                    tracing::debug!(
                        target: "kuncode::subagent",
                        name = %previous.name,
                        "workspace agent type overrides the global one",
                    );
                }
            }
        }
        catalog
    }

    /// Resolves a type by its registered name.
    pub fn resolve(&self, name: &str) -> Option<&AgentType> {
        self.builtins
            .iter()
            .find(|agent_type| agent_type.name == name)
            .or_else(|| self.custom.get(name))
    }

    /// Every type in advertised order: built-ins first, customs name-sorted.
    pub fn types(&self) -> impl Iterator<Item = &AgentType> {
        self.builtins.iter().chain(self.custom.values())
    }

    /// Number of custom (file-defined) types.
    pub fn custom_len(&self) -> usize {
        self.custom.len()
    }
}

/// Collects the valid definitions directly under one root, in deterministic
/// order.
fn scan_root(root: &Path) -> Vec<AgentType> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(
                target: "kuncode::subagent",
                root = %root.display(),
                diagnostic_chars = error.to_string().chars().count(),
                "agents root could not be read; skipping it",
            );
            return Vec::new();
        }
    };
    let mut documents: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("md")
        })
        .collect();
    // read_dir order is filesystem-dependent; the catalog re-sorts by name,
    // but determinism is cheap and keeps scans reproducible.
    documents.sort();
    documents
        .into_iter()
        .filter_map(|path| read_agent_type(&path))
        .collect()
}

/// Reads one definition file, or `None` when it holds no usable type.
fn read_agent_type(path: &Path) -> Option<AgentType> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) => {
            tracing::warn!(
                target: "kuncode::subagent",
                source = %path.display(),
                diagnostic_chars = error.to_string().chars().count(),
                "agent type definition could not be read; skipping it",
            );
            return None;
        }
    };
    let Ok(document) = String::from_utf8(raw) else {
        tracing::warn!(
            target: "kuncode::subagent",
            source = %path.display(),
            "agent type definition is not valid UTF-8; skipping it",
        );
        return None;
    };
    if document.trim().is_empty() {
        return None;
    }

    let parsed = frontmatter::parse(&document);
    // The file stem is the stable identity; frontmatter may override it but
    // an absent or blank field must not orphan the definition.
    let fallback_name = path.file_stem()?.to_str()?;
    let name = flatten(
        parsed.field("name").unwrap_or(fallback_name),
        MAX_NAME_CHARS,
    );
    if name.is_empty() {
        return None;
    }
    if name == GENERAL_AGENT_TYPE || name == FORK_AGENT_TYPE {
        tracing::warn!(
            target: "kuncode::subagent",
            source = %path.display(),
            name = %name,
            "agent type name is reserved for a built-in; skipping the file",
        );
        return None;
    }
    let description = flatten(
        parsed
            .field("description")
            .or_else(|| parsed.body().lines().find(|line| !line.trim().is_empty()))
            .unwrap_or(""),
        MAX_DESCRIPTION_CHARS,
    );
    let tools = parsed.field("tools").and_then(parse_tools);
    let instructions = match parsed.body().trim() {
        "" => None,
        body => {
            let (bounded, truncated) = bounded_body(body.to_string(), "agent profile");
            if truncated {
                tracing::warn!(
                    target: "kuncode::subagent",
                    source = %path.display(),
                    name = %name,
                    "agent type instructions exceed the document budget; truncated",
                );
            }
            Some(bounded)
        }
    };
    tracing::debug!(
        target: "kuncode::subagent",
        name = %name,
        source = %path.display(),
        restricted_tools = tools.as_ref().map_or(0, Vec::len),
        "agent type discovered",
    );
    Some(AgentType {
        name,
        description,
        context: SubagentContext::Fresh,
        tools,
        instructions,
    })
}

/// Parses the comma-separated `tools` whitelist; a blank field means no
/// restriction rather than "no tools", because an empty whitelist is far more
/// likely a formatting slip than an intent to delegate to a tool-less agent.
fn parse_tools(field: &str) -> Option<Vec<String>> {
    let tools: Vec<String> = field
        .split(',')
        .map(str::trim)
        .filter(|tool| !tool.is_empty())
        .map(str::to_string)
        .collect();
    (!tools.is_empty()).then_some(tools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Tests share a process, so the tag keeps concurrent cases apart.
    fn unique_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kuncode-agent-type-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_definition(root: &Path, file_name: &str, content: &str) {
        fs::write(root.join(file_name), content).expect("agent type definition");
    }

    #[test]
    fn the_builtin_catalog_holds_general_and_fork() {
        let catalog = AgentTypeCatalog::builtin();

        let names: Vec<&str> = catalog.types().map(AgentType::name).collect();
        assert_eq!(names, [GENERAL_AGENT_TYPE, FORK_AGENT_TYPE]);
        assert_eq!(
            catalog.resolve("general").expect("builtin").context(),
            SubagentContext::Fresh
        );
        let fork = catalog.resolve("fork").expect("builtin");
        assert_eq!(fork.context(), SubagentContext::Fork);
        // The framing that tells a fork the inherited instructions were the
        // parent's, not its own.
        assert!(
            fork.instructions()
                .expect("fork carries framing instructions")
                .contains("forked subagent")
        );
        assert_eq!(catalog.custom_len(), 0);
        assert!(catalog.resolve("absent").is_none());
    }

    #[test]
    fn scanned_definitions_parse_frontmatter_tools_and_instructions() {
        let root = unique_dir("parse");
        write_definition(
            &root,
            "explore.md",
            "---\nname: explore\ndescription: Read-only exploration.\ntools: read_file, grep,\n---\nOnly report findings; never edit files.",
        );

        let catalog = AgentTypeCatalog::scan(std::slice::from_ref(&root));
        let _ = fs::remove_dir_all(&root);

        let explore = catalog.resolve("explore").expect("scanned type");
        assert_eq!(explore.description(), "Read-only exploration.");
        assert_eq!(explore.context(), SubagentContext::Fresh);
        assert_eq!(
            explore.tools().expect("whitelist present"),
            ["read_file", "grep"]
        );
        assert_eq!(
            explore.instructions().expect("instructions present"),
            "Only report findings; never edit files."
        );
        // Advertised order: builtins first, then customs.
        let names: Vec<&str> = catalog.types().map(AgentType::name).collect();
        assert_eq!(names, ["general", "fork", "explore"]);
    }

    #[test]
    fn missing_frontmatter_falls_back_to_file_stem_and_first_line() {
        let root = unique_dir("fallback");
        write_definition(&root, "reviewer.md", "Reviews changes carefully.\nMore.");

        let catalog = AgentTypeCatalog::scan(std::slice::from_ref(&root));
        let _ = fs::remove_dir_all(&root);

        let reviewer = catalog.resolve("reviewer").expect("scanned type");
        assert_eq!(reviewer.description(), "Reviews changes carefully.");
        assert!(reviewer.tools().is_none());
    }

    #[test]
    fn reserved_names_and_unusable_files_are_skipped_not_fatal() {
        let root = unique_dir("reserved");
        write_definition(&root, "general.md", "---\nname: general\n---\nhijack");
        write_definition(&root, "renamed.md", "---\nname: fork\n---\nhijack");
        write_definition(&root, "blank.md", "  \n");
        fs::write(root.join("binary.md"), [0xff, 0xfe, 0x00]).expect("write");
        // Non-`.md` files and directories are not definitions.
        fs::write(root.join("notes.txt"), "not a type").expect("write");
        fs::create_dir_all(root.join("subdir.md")).expect("dir");
        write_definition(&root, "good.md", "usable");

        let catalog = AgentTypeCatalog::scan(std::slice::from_ref(&root));
        let _ = fs::remove_dir_all(&root);

        assert_eq!(catalog.custom_len(), 1);
        assert!(catalog.resolve("good").is_some());
        // The reserved names still resolve to the harness-owned built-ins.
        assert!(
            catalog
                .resolve("general")
                .expect("builtin")
                .instructions()
                .is_none()
        );
        assert_eq!(
            catalog.resolve("fork").expect("builtin").context(),
            SubagentContext::Fork
        );
    }

    #[test]
    fn later_roots_override_earlier_ones_by_name() {
        let global = unique_dir("override-global");
        let project = unique_dir("override-project");
        write_definition(&global, "explore.md", "global instructions");
        write_definition(&project, "explore.md", "project instructions");

        let catalog = AgentTypeCatalog::scan(&[global.clone(), project.clone()]);
        let _ = fs::remove_dir_all(&global);
        let _ = fs::remove_dir_all(&project);

        assert_eq!(catalog.custom_len(), 1);
        assert_eq!(
            catalog
                .resolve("explore")
                .expect("scanned type")
                .instructions(),
            Some("project instructions")
        );
    }

    #[test]
    fn a_blank_tools_field_means_no_restriction() {
        let root = unique_dir("blank-tools");
        write_definition(&root, "loose.md", "---\ntools: , ,\n---\nbody");

        let catalog = AgentTypeCatalog::scan(std::slice::from_ref(&root));
        let _ = fs::remove_dir_all(&root);

        assert!(
            catalog
                .resolve("loose")
                .expect("scanned type")
                .tools()
                .is_none()
        );
    }

    #[test]
    fn a_missing_root_scans_to_the_builtin_catalog() {
        let root = unique_dir("missing-root").join("does-not-exist");

        let catalog = AgentTypeCatalog::scan(&[root]);

        assert_eq!(catalog.custom_len(), 0);
        assert!(catalog.resolve(GENERAL_AGENT_TYPE).is_some());
    }
}
