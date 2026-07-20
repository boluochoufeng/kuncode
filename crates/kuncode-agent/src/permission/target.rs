//! Typed, canonical resources that permission rules can match.

use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use thiserror::Error;
use url::Url;

const MAX_COMMAND_CHARS: usize = 4_096;
const MAX_ORIGIN_CHARS: usize = 4_096;
const MAX_PATH_PATTERN_CHARS: usize = 4_096;
const MAX_IDENTITY_CHARS: usize = 256;
const MAX_MCP_FIELD_NAME_CHARS: usize = 128;
const MAX_MCP_FIELD_VALUE_CHARS: usize = 1_024;
const MAX_MCP_FIELDS: usize = 32;

/// Stable permission namespace selected by trusted tool registration.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionNamespace {
    /// Reads file content or lists filesystem state.
    Read,
    /// Creates, changes, moves, or removes filesystem state.
    Edit,
    /// Starts a local shell command.
    Bash,
    /// Fetches a network origin.
    WebFetch,
    /// Invokes a remote MCP tool.
    Mcp,
    /// Starts a registered agent profile.
    Agent,
    /// Mutates the session-owned task plan.
    TodoWrite,
    /// Safely identifies a tool without a more specific trusted adapter.
    ExactTool,
}

/// Absolute UTF-8 path used by both authorization and execution.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CanonicalPath(String);

impl CanonicalPath {
    pub(crate) fn fail_closed_anchor() -> Self {
        Self("/".to_string())
    }

    /// Converts an absolute resolved path into its stable permission form.
    ///
    /// # Errors
    /// Returns an error for relative or non-UTF-8 paths.
    pub fn from_absolute(path: &Path) -> Result<Self, PermissionTargetError> {
        if !path.is_absolute() {
            return Err(PermissionTargetError::RelativePath {
                path: path.display().to_string(),
            });
        }
        let normalized = normalize_absolute_path(path)?;
        let value = normalized
            .to_str()
            .ok_or(PermissionTargetError::NonUtf8Path)?
            .replace('\\', "/");
        Ok(Self(value))
    }

    /// Returns the canonical path text used by namespace matchers.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A concrete path or a workspace-rooted path pattern.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PathSelector {
    /// One resolved filesystem path.
    Exact {
        /// Canonical path shared with the prepared invocation.
        path: CanonicalPath,
    },
    /// A normalized pattern evaluated under one canonical root.
    Pattern {
        /// Root that bounds pattern expansion.
        root: CanonicalPath,
        /// Slash-form pattern with no parent traversal.
        pattern: String,
    },
}

impl PathSelector {
    /// Creates a selector for one resolved path.
    pub fn exact(path: CanonicalPath) -> Self {
        Self::Exact { path }
    }

    /// Creates a root-bounded pattern selector.
    ///
    /// # Errors
    /// Returns an error when the pattern is blank, absolute, or contains a
    /// parent-traversal segment.
    pub fn pattern(
        root: CanonicalPath,
        pattern: impl Into<String>,
    ) -> Result<Self, PermissionTargetError> {
        let pattern = normalize_relative_pattern(&pattern.into())?;
        if pattern.trim().is_empty() {
            return Err(PermissionTargetError::BlankSelector("path pattern"));
        }
        ensure_bounded(&pattern, "path pattern", MAX_PATH_PATTERN_CHARS)?;
        Ok(Self::Pattern { root, pattern })
    }
}

/// How confidently the built-in parser understands a shell command.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    /// A single command whose argv boundaries were parsed conservatively.
    Simple,
    /// Shell syntax whose effects cannot be represented safely as argv.
    Opaque,
}

/// Canonical command selector retained exactly for authorization and execution.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CanonicalCommand {
    text: String,
    kind: CommandKind,
}

impl CanonicalCommand {
    /// Builds a non-empty command selector.
    ///
    /// # Errors
    /// Returns an error for a blank command.
    pub fn new(text: impl Into<String>, kind: CommandKind) -> Result<Self, PermissionTargetError> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(PermissionTargetError::BlankSelector("command"));
        }
        ensure_bounded(&text, "command", MAX_COMMAND_CHARS)?;
        Ok(Self { text, kind })
    }

    /// Returns the command text passed to the prepared invocation.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Returns whether the command was parsed or kept opaque.
    pub const fn kind(&self) -> CommandKind {
        self.kind
    }
}

/// Canonical network origin.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CanonicalOrigin(String);

impl CanonicalOrigin {
    /// Extracts a canonical HTTP(S) origin from a complete URL or origin.
    ///
    /// # Errors
    /// Returns an error for malformed URLs, unsupported schemes, missing hosts,
    /// or embedded credentials.
    pub fn new(value: impl Into<String>) -> Result<Self, PermissionTargetError> {
        let value = non_blank(value.into(), "origin")?;
        ensure_bounded(&value, "origin", MAX_ORIGIN_CHARS)?;
        let parsed = Url::parse(&value).map_err(|_| PermissionTargetError::InvalidOrigin)?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(PermissionTargetError::InvalidOrigin);
        }
        Ok(Self(parsed.origin().ascii_serialization()))
    }

    /// Returns the canonical origin text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Exact MCP server/tool selector with optional trusted field projection.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct McpSelector {
    server: String,
    tool: String,
    fields: Vec<(String, String)>,
}

impl McpSelector {
    /// Builds an exact selector and sorts trusted projected fields.
    ///
    /// # Errors
    /// Returns an error for blank server, tool, field name, or field value.
    pub fn new(
        server: impl Into<String>,
        tool: impl Into<String>,
        fields: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, PermissionTargetError> {
        let server = non_blank(server.into(), "MCP server")?;
        ensure_bounded(&server, "MCP server", MAX_IDENTITY_CHARS)?;
        let tool = non_blank(tool.into(), "MCP tool")?;
        ensure_bounded(&tool, "MCP tool", MAX_IDENTITY_CHARS)?;
        let mut fields = fields.into_iter().collect::<Vec<_>>();
        if fields.len() > MAX_MCP_FIELDS {
            return Err(PermissionTargetError::TooManyMcpFields {
                actual: fields.len(),
                maximum: MAX_MCP_FIELDS,
            });
        }
        for (name, value) in &fields {
            non_blank(name.clone(), "MCP field name")?;
            non_blank(value.clone(), "MCP field value")?;
            ensure_bounded(name, "MCP field name", MAX_MCP_FIELD_NAME_CHARS)?;
            ensure_bounded(value, "MCP field value", MAX_MCP_FIELD_VALUE_CHARS)?;
        }
        fields.sort();
        fields.dedup();
        if fields
            .windows(2)
            .any(|pair| pair[0].0 == pair[1].0 && pair[0].1 != pair[1].1)
        {
            return Err(PermissionTargetError::ConflictingMcpField);
        }
        Ok(Self {
            server,
            tool,
            fields,
        })
    }

    /// Returns the exact registered server name.
    pub fn server(&self) -> &str {
        &self.server
    }

    /// Returns the exact remote tool name.
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// Returns trusted projected fields in canonical order.
    pub fn fields(&self) -> &[(String, String)] {
        &self.fields
    }
}

/// One canonical resource requiring a permission decision.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "namespace", content = "selector", rename_all = "snake_case")]
pub enum PermissionTarget {
    /// Filesystem read target.
    Read(PathSelector),
    /// Filesystem edit target.
    Edit(PathSelector),
    /// Local shell target.
    Bash(CanonicalCommand),
    /// Network origin target.
    WebFetch(CanonicalOrigin),
    /// Remote MCP target.
    Mcp(McpSelector),
    /// Registered agent profile name.
    Agent(String),
    /// Session task-plan mutation.
    TodoWrite,
    /// Exact fallback tool identity.
    ExactTool(String),
}

impl PermissionTarget {
    /// Returns the namespace used to validate profiles and compile rules.
    pub const fn namespace(&self) -> PermissionNamespace {
        match self {
            Self::Read(_) => PermissionNamespace::Read,
            Self::Edit(_) => PermissionNamespace::Edit,
            Self::Bash(_) => PermissionNamespace::Bash,
            Self::WebFetch(_) => PermissionNamespace::WebFetch,
            Self::Mcp(_) => PermissionNamespace::Mcp,
            Self::Agent(_) => PermissionNamespace::Agent,
            Self::TodoWrite => PermissionNamespace::TodoWrite,
            Self::ExactTool(_) => PermissionNamespace::ExactTool,
        }
    }

    /// Builds an exact fallback target after validating the tool name.
    ///
    /// # Errors
    /// Returns an error for a blank name.
    pub fn exact_tool(name: impl Into<String>) -> Result<Self, PermissionTargetError> {
        let name = non_blank(name.into(), "tool name")?;
        ensure_bounded(&name, "tool name", MAX_IDENTITY_CHARS)?;
        Ok(Self::ExactTool(name))
    }

    /// Builds an agent target after validating the profile name.
    ///
    /// # Errors
    /// Returns an error for a blank profile.
    pub fn agent(profile: impl Into<String>) -> Result<Self, PermissionTargetError> {
        let profile = non_blank(profile.into(), "agent profile")?;
        ensure_bounded(&profile, "agent profile", MAX_IDENTITY_CHARS)?;
        Ok(Self::Agent(profile))
    }

    pub(crate) fn validate(&self) -> Result<(), PermissionTargetError> {
        match self {
            Self::Agent(profile) => {
                non_blank(profile.clone(), "agent profile")?;
                ensure_bounded(profile, "agent profile", MAX_IDENTITY_CHARS)
            }
            Self::ExactTool(tool) => {
                non_blank(tool.clone(), "tool name")?;
                ensure_bounded(tool, "tool name", MAX_IDENTITY_CHARS)
            }
            _ => Ok(()),
        }
    }
}

impl std::fmt::Display for PermissionTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(selector) => write!(f, "Read({})", path_selector_display(selector)),
            Self::Edit(selector) => write!(f, "Edit({})", path_selector_display(selector)),
            Self::Bash(command) => match command.kind() {
                CommandKind::Simple => write!(f, "Bash({})", safe_ui_text(command.as_str())),
                CommandKind::Opaque => {
                    write!(
                        f,
                        "Bash(opaque/unable to fully analyze: {})",
                        safe_ui_text(command.as_str())
                    )
                }
            },
            Self::WebFetch(origin) => write!(f, "WebFetch({})", origin.as_str()),
            Self::Mcp(selector) => {
                write!(f, "Mcp({}.{}", selector.server(), selector.tool())?;
                for (name, value) in selector.fields() {
                    write!(f, ", {}={}", safe_ui_text(name), safe_ui_text(value))?;
                }
                f.write_str(")")
            }
            Self::Agent(profile) => write!(f, "Agent({})", safe_ui_text(profile)),
            Self::TodoWrite => f.write_str("TodoWrite"),
            Self::ExactTool(tool) => write!(f, "ExactTool({})", safe_ui_text(tool)),
        }
    }
}

fn path_selector_display(selector: &PathSelector) -> String {
    match selector {
        PathSelector::Exact { path } => safe_ui_text(path.as_str()),
        PathSelector::Pattern { root, pattern } => safe_ui_text(&format!(
            "{}/{pattern}",
            root.as_str().trim_end_matches('/')
        )),
    }
}

fn safe_ui_text(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| {
            if ch.is_control() {
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect()
}

/// Invalid canonical target emitted by a tool adapter.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PermissionTargetError {
    /// A selector that must carry a value was blank.
    #[error("{0} must not be blank")]
    BlankSelector(&'static str),
    /// Permission paths must already be absolute.
    #[error("permission path `{path}` is not absolute")]
    RelativePath {
        /// Rejected display form.
        path: String,
    },
    /// JSON tool input cannot safely name a non-UTF-8 canonical path.
    #[error("canonical permission path is not valid UTF-8")]
    NonUtf8Path,
    /// Lexical normalization attempted to traverse above the filesystem root.
    #[error("permission path `{path}` traverses above its root")]
    ParentTraversal {
        /// Rejected display form.
        path: String,
    },
    /// A pattern could escape its registered root.
    #[error("path pattern `{0}` is absolute or contains parent traversal")]
    UnsafePathPattern(String),
    /// A value was not a canonicalizable HTTP(S) origin.
    #[error("value is not a valid credential-free HTTP(S) origin")]
    InvalidOrigin,
    /// One projected MCP field cannot carry several values.
    #[error("MCP selector contains conflicting values for one projected field")]
    ConflictingMcpField,
    /// Security-sensitive selectors remain bounded for matching and display.
    #[error("{label} exceeds the maximum of {maximum} characters")]
    SelectorTooLong {
        /// Selector category, never the rejected value itself.
        label: &'static str,
        /// Trusted product limit.
        maximum: usize,
    },
    /// Trusted MCP projections remain bounded.
    #[error("MCP selector has {actual} projected fields, exceeding the maximum {maximum}")]
    TooManyMcpFields {
        /// Number supplied by the adapter.
        actual: usize,
        /// Trusted product limit.
        maximum: usize,
    },
}

fn normalize_absolute_path(path: &Path) -> Result<PathBuf, PermissionTargetError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(PermissionTargetError::ParentTraversal {
                        path: path.display().to_string(),
                    });
                }
            }
        }
    }
    Ok(normalized)
}

fn normalize_relative_pattern(pattern: &str) -> Result<String, PermissionTargetError> {
    let pattern = pattern.replace('\\', "/");
    if pattern.starts_with('/') {
        return Err(PermissionTargetError::UnsafePathPattern(pattern));
    }
    let mut segments = Vec::new();
    for segment in pattern.split('/') {
        match segment {
            "" | "." => {}
            ".." => return Err(PermissionTargetError::UnsafePathPattern(pattern)),
            segment => segments.push(segment),
        }
    }
    Ok(segments.join("/"))
}

fn non_blank(value: String, label: &'static str) -> Result<String, PermissionTargetError> {
    if value.trim().is_empty() {
        Err(PermissionTargetError::BlankSelector(label))
    } else {
        Ok(value)
    }
}

fn ensure_bounded(
    value: &str,
    label: &'static str,
    maximum: usize,
) -> Result<(), PermissionTargetError> {
    if value.chars().take(maximum.saturating_add(1)).count() > maximum {
        Err(PermissionTargetError::SelectorTooLong { label, maximum })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_paths_require_absolute_utf8_input() {
        assert!(matches!(
            CanonicalPath::from_absolute(Path::new("src/lib.rs")),
            Err(PermissionTargetError::RelativePath { .. })
        ));
        let root = std::env::current_dir().expect("current directory exists");
        let path = CanonicalPath::from_absolute(&root).expect("absolute UTF-8 path");
        assert!(!path.as_str().is_empty());
    }

    #[test]
    fn patterns_reject_parent_traversal() {
        let root = CanonicalPath::from_absolute(
            &std::env::current_dir().expect("current directory exists"),
        )
        .expect("root is canonical");
        assert!(matches!(
            PathSelector::pattern(root, "src/../secrets/**"),
            Err(PermissionTargetError::UnsafePathPattern(_))
        ));
    }

    #[test]
    fn canonical_paths_remove_lexical_noise() {
        let path = CanonicalPath::from_absolute(Path::new("/workspace/./src/../lib.rs"))
            .expect("absolute path normalizes");
        assert_eq!(path.as_str(), "/workspace/lib.rs");
    }

    #[test]
    fn origins_drop_paths_queries_and_default_ports() {
        let detailed = CanonicalOrigin::new("HTTPS://EXAMPLE.COM:443/a?token=secret#fragment")
            .expect("valid URL");
        let plain = CanonicalOrigin::new("https://example.com").expect("valid origin");
        assert_eq!(detailed, plain);
        assert_eq!(detailed.as_str(), "https://example.com");
        assert_eq!(
            CanonicalOrigin::new("https://example.com:8443/path")
                .expect("valid non-default port")
                .as_str(),
            "https://example.com:8443"
        );
    }

    #[test]
    fn origins_reject_credentials_and_non_http_schemes() {
        assert!(matches!(
            CanonicalOrigin::new("https://user:secret@example.com"),
            Err(PermissionTargetError::InvalidOrigin)
        ));
        assert!(matches!(
            CanonicalOrigin::new("file:///etc/passwd"),
            Err(PermissionTargetError::InvalidOrigin)
        ));
    }

    #[test]
    fn mcp_fields_have_deterministic_order() {
        let left = McpSelector::new(
            "github",
            "create_issue",
            [
                ("repo".to_string(), "a/b".to_string()),
                ("org".to_string(), "a".to_string()),
            ],
        )
        .expect("valid selector");
        let right = McpSelector::new(
            "github",
            "create_issue",
            [
                ("org".to_string(), "a".to_string()),
                ("repo".to_string(), "a/b".to_string()),
            ],
        )
        .expect("valid selector");
        assert_eq!(left, right);
    }

    #[test]
    fn mcp_fields_reject_conflicting_projections() {
        assert!(matches!(
            McpSelector::new(
                "github",
                "create_issue",
                [
                    ("repo".to_string(), "a/one".to_string()),
                    ("repo".to_string(), "a/two".to_string()),
                ],
            ),
            Err(PermissionTargetError::ConflictingMcpField)
        ));
    }

    #[test]
    fn command_and_pattern_selectors_are_bounded() {
        assert!(matches!(
            CanonicalCommand::new("x".repeat(MAX_COMMAND_CHARS + 1), CommandKind::Opaque),
            Err(PermissionTargetError::SelectorTooLong {
                label: "command",
                ..
            })
        ));
        let root = CanonicalPath::from_absolute(Path::new("/workspace")).expect("valid root");
        assert!(matches!(
            PathSelector::pattern(root, "x".repeat(MAX_PATH_PATTERN_CHARS + 1)),
            Err(PermissionTargetError::SelectorTooLong {
                label: "path pattern",
                ..
            })
        ));
    }
}
