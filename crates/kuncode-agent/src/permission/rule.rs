//! Permission rule grammar, alias expansion, and matching.
//!
//! A rule is `Tool` or `Tool(Resource)`. A bare `Tool` matches any call to that
//! tool; `Tool(Resource)` additionally requires the call's resource to match the
//! `Resource` glob. Matching reuses the pure [`crate::glob`] matcher — paths use
//! segmented [`glob_match`], commands use flat [`command_match`] — and never
//! touches the filesystem.

use thiserror::Error;

use crate::glob::{command_match, glob_match, normalize_pattern};

use super::request::{PermissionAction, PermissionRequest};

/// Where a rule came from, for "why am I being asked / blocked" explainability.
/// Also preserves the rule's authored intent across alias expansion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleOrigin {
    /// Shipped with the harness (e.g. the retired bash blocklist).
    Builtin,
    /// Loaded from `.kuncode/settings.json`.
    ProjectSettings,
    /// Passed on the command line (`--allow` / `--deny` / `--ask`).
    CliFlag,
    /// Granted by the user during an approval prompt ("Always …").
    SessionGrant,
}

/// One canonical permission rule. Aliases (`Read`, `Edit`, …) are expanded into
/// one or more of these at parse time, so matching only ever deals with
/// canonical tool names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// Canonical tool name this rule applies to, e.g. `"read_file"`.
    pub tool: String,
    /// Resource glob; `None` is a bare-tool rule matching any resource.
    pub resource: Option<String>,
    /// Provenance for explainability.
    pub origin: RuleOrigin,
    /// Original authored text (pre-expansion), e.g. `"Read(*)"`.
    pub raw: String,
}

impl Rule {
    /// Returns `true` when this rule applies to `req`. The resource matcher is
    /// chosen by the request's action: `Execute` resources are flat commands,
    /// `Read`/`Write` resources are `/`-segmented paths.
    pub fn matches(&self, req: &PermissionRequest) -> bool {
        if self.tool != req.tool {
            return false;
        }
        match (&self.resource, &req.resource) {
            // Bare-tool rule: matches any resource (or none).
            (None, _) => true,
            // Rule wants a specific resource but the call carries none.
            (Some(_), None) => false,
            (Some(pattern), Some(resource)) => match req.action {
                PermissionAction::Execute => command_match(pattern, resource),
                // `Meta` carries no resource, so it never reaches this arm; group
                // it with the path matcher to keep the match exhaustive.
                PermissionAction::Read | PermissionAction::Write | PermissionAction::Meta => {
                    glob_match(&normalize_pattern(pattern), &normalize_pattern(resource))
                }
            },
        }
    }
}

/// Returns the first rule in `rules` that matches `req`, if any.
pub fn first_match<'a>(rules: &'a [Rule], req: &PermissionRequest) -> Option<&'a Rule> {
    rules.iter().find(|rule| rule.matches(req))
}

/// Returns `true` when any rule in `rules` matches `req`.
pub fn matches_any(rules: &[Rule], req: &PermissionRequest) -> bool {
    first_match(rules, req).is_some()
}

/// Errors raised while parsing a rule string.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RuleParseError {
    /// The rule text was empty or whitespace.
    #[error("rule is empty")]
    Empty,
    /// A `(` was opened but the rule did not end with `)`.
    #[error("rule `{0}` has an unbalanced parenthesis")]
    Unbalanced(String),
    /// The tool name before `(` was empty.
    #[error("rule `{0}` is missing a tool name")]
    MissingTool(String),
    /// The resource inside `(...)` was empty.
    #[error("rule `{0}` has an empty resource")]
    EmptyResource(String),
}

/// Parses one rule string into one or more canonical [`Rule`]s.
///
/// Aliases expand to several rules (`Read` → `read_file` + `glob`), so this
/// returns a `Vec`. Canonical tool names pass through unchanged.
pub fn parse_rule(input: &str, origin: RuleOrigin) -> Result<Vec<Rule>, RuleParseError> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(RuleParseError::Empty);
    }

    let (tool_part, resource) = match raw.find('(') {
        Some(open) => {
            if !raw.ends_with(')') {
                return Err(RuleParseError::Unbalanced(raw.to_string()));
            }
            let close = raw.len() - 1; // ')' is ASCII, so this is a char boundary.
            let tool = raw[..open].trim();
            let resource = raw[open + 1..close].trim();
            if resource.is_empty() {
                return Err(RuleParseError::EmptyResource(raw.to_string()));
            }
            (tool, Some(resource.to_string()))
        }
        None => (raw, None),
    };

    if tool_part.is_empty() {
        return Err(RuleParseError::MissingTool(raw.to_string()));
    }

    let raw_owned = raw.to_string();
    Ok(expand_alias(tool_part)
        .into_iter()
        .map(|tool| Rule {
            tool,
            resource: resource.clone(),
            origin,
            raw: raw_owned.clone(),
        })
        .collect())
}

/// Proposes a sensible "Always allow/deny" scope for a request, to offer the
/// user at an approval prompt. Deliberately curbed: commands grant a *prefix*
/// (e.g. `bash(cargo*)`), never `bash(*)`; file calls grant the specific path;
/// resourceless calls grant the whole tool. The user can always narrow it.
pub fn suggest_scope(req: &PermissionRequest) -> Rule {
    let tool = req.tool.clone();
    match req.action {
        PermissionAction::Execute => {
            // Scope to the command's first word (its program), e.g.
            // `cargo build` → `bash(cargo*)`. Never widen to a blanket
            // `bash(*)`: an empty command has no program to scope, so fall back
            // to the exact (empty) command — a grant that can only ever
            // re-allow empty commands, which `run()` rejects anyway.
            let command = req.resource.as_deref().unwrap_or("").trim();
            let first_word = command.split_whitespace().next().unwrap_or("");
            let resource = if first_word.is_empty() {
                command.to_string()
            } else {
                format!("{first_word}*")
            };
            Rule {
                raw: format!("{tool}({resource})"),
                resource: Some(resource),
                tool,
                origin: RuleOrigin::SessionGrant,
            }
        }
        // `Meta` is allow-by-default and never asks, so `suggest_scope` is never
        // called for it; grouped here only to keep the match exhaustive — a
        // resourceless `Meta` would fall to the bare-tool grant below.
        PermissionAction::Read | PermissionAction::Write | PermissionAction::Meta => {
            match &req.resource {
                Some(resource) => Rule {
                    raw: format!("{tool}({resource})"),
                    resource: Some(resource.clone()),
                    tool,
                    origin: RuleOrigin::SessionGrant,
                },
                None => Rule {
                    raw: tool.clone(),
                    resource: None,
                    tool,
                    origin: RuleOrigin::SessionGrant,
                },
            }
        }
    }
}

/// Expands a (possibly aliased) tool name into canonical tool names. The table
/// is curated and not user-extensible; unknown names pass through as-is so a
/// rule can also name a canonical tool directly.
fn expand_alias(tool: &str) -> Vec<String> {
    match tool {
        "Bash" => vec!["bash".to_string()],
        "Read" => vec!["read_file".to_string(), "glob".to_string()],
        // The file-mutation family: users think "can it edit files", not which
        // of the two write tools.
        "Edit" | "Write" => vec!["write_file".to_string(), "edit_file".to_string()],
        other => vec![other.to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::request::PermissionRequest;

    fn req(tool: &str, action: PermissionAction, resource: Option<&str>) -> PermissionRequest {
        PermissionRequest::new(tool, action, resource.map(str::to_string), "test")
    }

    #[test]
    fn read_alias_expands_to_read_file_and_glob() {
        let rules = parse_rule("Read", RuleOrigin::CliFlag).expect("parses");
        let tools: Vec<_> = rules.iter().map(|r| r.tool.as_str()).collect();
        assert_eq!(tools, ["read_file", "glob"]);
        // The authored text is preserved on every expanded rule.
        assert!(rules.iter().all(|r| r.raw == "Read"));
    }

    #[test]
    fn bare_tool_rule_matches_any_resource() {
        let rules = parse_rule("Bash", RuleOrigin::CliFlag).expect("parses");
        assert!(rules[0].matches(&req("bash", PermissionAction::Execute, Some("anything"))));
    }

    #[test]
    fn resource_rule_requires_resource_to_match() {
        let rules = parse_rule("Edit(src/**)", RuleOrigin::ProjectSettings).expect("parses");
        // Both write tools come from the alias.
        assert_eq!(rules.len(), 2);
        let edits_src = req("edit_file", PermissionAction::Write, Some("src/lib.rs"));
        let edits_root = req("edit_file", PermissionAction::Write, Some("Cargo.toml"));
        assert!(rules.iter().any(|r| r.matches(&edits_src)));
        assert!(!rules.iter().any(|r| r.matches(&edits_root)));
    }

    #[test]
    fn command_rule_spans_path_slashes() {
        let rules = parse_rule("Bash(sudo*)", RuleOrigin::Builtin).expect("parses");
        assert!(rules[0].matches(&req(
            "bash",
            PermissionAction::Execute,
            Some("sudo rm -rf /home")
        )));
        assert!(!rules[0].matches(&req("bash", PermissionAction::Execute, Some("ls -la"))));
    }

    #[test]
    fn resource_rule_does_not_match_resourceless_call() {
        let rules = parse_rule("glob(*.rs)", RuleOrigin::CliFlag).expect("parses");
        assert!(!rules[0].matches(&req("glob", PermissionAction::Read, None)));
    }

    #[test]
    fn suggest_scope_grants_command_prefix_not_star() {
        let scope = suggest_scope(&req(
            "bash",
            PermissionAction::Execute,
            Some("cargo build --release"),
        ));
        assert_eq!(scope.raw, "bash(cargo*)");
        assert_eq!(scope.origin, RuleOrigin::SessionGrant);
        // It matches the cargo family but is not a blanket `bash(*)`.
        assert!(scope.matches(&req("bash", PermissionAction::Execute, Some("cargo test"))));
        assert!(!scope.matches(&req("bash", PermissionAction::Execute, Some("rm -rf x"))));
    }

    #[test]
    fn suggest_scope_never_blankets_an_empty_command() {
        // A whitespace-only command has no program to scope to; the suggested
        // grant must NOT widen to `bash(*)`. It falls back to the exact (empty)
        // command, which only ever re-allows empty commands.
        let scope = suggest_scope(&req("bash", PermissionAction::Execute, Some("   ")));
        assert_eq!(scope.raw, "bash()");
        assert!(!scope.matches(&req("bash", PermissionAction::Execute, Some("rm -rf /"))));
    }

    #[test]
    fn suggest_scope_grants_specific_file_path() {
        let scope = suggest_scope(&req(
            "edit_file",
            PermissionAction::Write,
            Some("src/lib.rs"),
        ));
        assert_eq!(scope.raw, "edit_file(src/lib.rs)");
        assert!(scope.matches(&req(
            "edit_file",
            PermissionAction::Write,
            Some("src/lib.rs")
        )));
        assert!(!scope.matches(&req(
            "edit_file",
            PermissionAction::Write,
            Some("src/main.rs")
        )));
    }

    #[test]
    fn rejects_malformed_rules() {
        assert_eq!(
            parse_rule("  ", RuleOrigin::CliFlag),
            Err(RuleParseError::Empty)
        );
        assert!(matches!(
            parse_rule("Bash(", RuleOrigin::CliFlag),
            Err(RuleParseError::Unbalanced(_))
        ));
        assert!(matches!(
            parse_rule("Bash()", RuleOrigin::CliFlag),
            Err(RuleParseError::EmptyResource(_))
        ));
        assert!(matches!(
            parse_rule("(foo)", RuleOrigin::CliFlag),
            Err(RuleParseError::MissingTool(_))
        ));
    }
}
