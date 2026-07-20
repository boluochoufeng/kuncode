//! Compiles effect-bearing permission rules into namespace-specific matchers.

use serde::Serialize;
use thiserror::Error;
use url::Host;

use crate::glob::{command_match, glob_match, normalize_pattern};

use super::{
    CanonicalPath, CommandKind, PathSelector, PermissionCauseId, PermissionCheck, PermissionTarget,
    PolicyContribution, PolicyEffect, PolicyOrigin, SafeExplanation,
};

const MAX_RULE_CHARS: usize = 4_096;

/// Workspace anchor required to compile relative path rules deterministically.
#[derive(Clone, Debug)]
pub struct RuleCompileContext {
    workspace_root: CanonicalPath,
}

impl RuleCompileContext {
    /// Creates a compiler context from the canonical workspace root.
    pub fn new(workspace_root: CanonicalPath) -> Self {
        Self { workspace_root }
    }

    /// Returns the anchor used for relative Read/Edit rules.
    pub fn workspace_root(&self) -> &CanonicalPath {
        &self.workspace_root
    }
}

/// Canonical, namespace-specific rule used by the product policy engine.
#[derive(Clone, Debug)]
pub struct PermissionRule {
    effect: PolicyEffect,
    origin: PolicyOrigin,
    matcher: PermissionMatcher,
    cause_id: PermissionCauseId,
    explanation: SafeExplanation,
}

impl PermissionRule {
    pub(crate) fn fail_closed() -> Self {
        Self {
            effect: PolicyEffect::Deny,
            origin: PolicyOrigin::Builtin,
            matcher: PermissionMatcher::All,
            cause_id: PermissionCauseId::from_trusted_bytes(b"policy-initialization-failed"),
            explanation: SafeExplanation::new("permission policy failed to initialize"),
        }
    }

    /// Builds an exact target rule for a challenge-generated mutation option.
    ///
    /// This path never accepts UI-authored matcher text, preventing approval
    /// frontends from widening the engine-selected scope.
    pub(crate) fn exact_target(
        target: PermissionTarget,
        effect: PolicyEffect,
        origin: PolicyOrigin,
    ) -> Result<Self, serde_json::Error> {
        let matcher = PermissionMatcher::Exact {
            target: target.clone(),
        };
        #[derive(Serialize)]
        struct CausePayload<'a> {
            kind: &'static str,
            effect: PolicyEffect,
            origin: &'a PolicyOrigin,
            target: &'a PermissionTarget,
        }
        let cause_id = PermissionCauseId::derive(&CausePayload {
            kind: "exact_approval_template",
            effect,
            origin: &origin,
            target: &target,
        })?;
        Ok(Self {
            effect,
            explanation: SafeExplanation::new("challenge-generated exact permission rule"),
            origin,
            matcher,
            cause_id,
        })
    }

    /// Returns the configured effect.
    pub const fn effect(&self) -> PolicyEffect {
        self.effect
    }

    /// Returns trusted provenance.
    pub fn origin(&self) -> &PolicyOrigin {
        &self.origin
    }

    /// Returns the stable rule cause.
    pub fn cause_id(&self) -> &PermissionCauseId {
        &self.cause_id
    }

    /// Produces a contribution when this rule matches `check`.
    pub fn contribution(&self, check: &PermissionCheck) -> Option<PolicyContribution> {
        self.matcher.matches(check.target(), self.effect).then(|| {
            PolicyContribution::new(
                self.effect,
                self.origin.clone(),
                self.cause_id.clone(),
                self.explanation.clone(),
            )
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "namespace", rename_all = "snake_case")]
enum PermissionMatcher {
    All,
    Read(PathMatcher),
    Edit(PathMatcher),
    Bash { pattern: String },
    WebFetch { domain: String },
    Mcp { server: String, tool: String },
    Agent { profile: String },
    TodoWrite,
    ExactTool { tool: String },
    Exact { target: PermissionTarget },
}

impl PermissionMatcher {
    fn matches(&self, target: &PermissionTarget, effect: PolicyEffect) -> bool {
        match (self, target) {
            (Self::All, _) => true,
            (Self::Read(rule), PermissionTarget::Read(target))
            | (Self::Edit(rule), PermissionTarget::Edit(target)) => rule.matches(target),
            (Self::Bash { pattern }, PermissionTarget::Bash(command)) => {
                if effect == PolicyEffect::Allow && command.kind() == CommandKind::Opaque {
                    !contains_wildcard(pattern) && pattern == command.as_str()
                } else {
                    command_match(pattern, command.as_str())
                }
            }
            (Self::WebFetch { domain }, PermissionTarget::WebFetch(origin)) => {
                origin_domain(origin.as_str()).is_some_and(|host| domain_matches(domain, host))
            }
            (Self::Mcp { server, tool }, PermissionTarget::Mcp(selector)) => {
                server == selector.server() && tool == selector.tool()
            }
            (Self::Agent { profile }, PermissionTarget::Agent(target)) => profile == target,
            (Self::TodoWrite, PermissionTarget::TodoWrite) => true,
            (Self::ExactTool { tool }, PermissionTarget::ExactTool(target)) => tool == target,
            (Self::Exact { target: expected }, target) => expected == target,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct PathMatcher {
    anchor: Option<CanonicalPath>,
    pattern: String,
}

impl PathMatcher {
    fn matches(&self, target: &PathSelector) -> bool {
        match target {
            PathSelector::Exact { path } => self.matches_path(path),
            PathSelector::Pattern { root, pattern } => {
                self.anchor.as_ref() == Some(root)
                    && (self.pattern == "**" || self.pattern == *pattern)
            }
        }
    }

    fn matches_path(&self, path: &CanonicalPath) -> bool {
        let candidate = match &self.anchor {
            Some(anchor) => {
                let Some(relative) = strip_path_prefix(path.as_str(), anchor.as_str()) else {
                    return false;
                };
                relative
            }
            None => path.as_str(),
        };
        glob_match(
            &normalize_pattern(&self.pattern),
            &normalize_pattern(candidate),
        )
    }
}

/// Compiles one effect-bearing rule using its namespace-specific grammar.
///
/// # Errors
/// Returns a typed error for malformed or unsupported syntax.
pub fn compile_permission_rule(
    input: &str,
    effect: PolicyEffect,
    origin: PolicyOrigin,
    context: &RuleCompileContext,
) -> Result<PermissionRule, PermissionRuleError> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(PermissionRuleError::Empty);
    }
    if raw.chars().take(MAX_RULE_CHARS + 1).count() > MAX_RULE_CHARS {
        return Err(PermissionRuleError::TooLong {
            maximum: MAX_RULE_CHARS,
        });
    }

    let matcher = if raw == "TodoWrite" {
        PermissionMatcher::TodoWrite
    } else if let Some(rest) = raw.strip_prefix("mcp.") {
        let (server, tool) = rest
            .split_once('.')
            .ok_or_else(|| PermissionRuleError::InvalidMcp(raw.to_string()))?;
        if server.trim().is_empty() || tool.trim().is_empty() || tool.contains('.') {
            return Err(PermissionRuleError::InvalidMcp(raw.to_string()));
        }
        PermissionMatcher::Mcp {
            server: server.to_string(),
            tool: tool.to_string(),
        }
    } else {
        let (namespace, selector) = parse_call_syntax(raw)?;
        match namespace {
            "Read" => PermissionMatcher::Read(compile_path_matcher(selector, context)?),
            "Edit" => PermissionMatcher::Edit(compile_path_matcher(selector, context)?),
            "Bash" => PermissionMatcher::Bash {
                pattern: non_blank_selector(namespace, selector)?.to_string(),
            },
            "WebFetch" => {
                let selector = non_blank_selector(namespace, selector)?;
                let domain = selector.strip_prefix("domain:").ok_or_else(|| {
                    PermissionRuleError::InvalidSelector {
                        namespace: namespace.to_string(),
                        selector: selector.to_string(),
                    }
                })?;
                let domain = match Host::parse(domain) {
                    Ok(Host::Domain(domain)) => domain.trim_end_matches('.').to_string(),
                    _ => {
                        return Err(PermissionRuleError::InvalidSelector {
                            namespace: namespace.to_string(),
                            selector: selector.to_string(),
                        });
                    }
                };
                if !valid_dns_domain(&domain) {
                    return Err(PermissionRuleError::InvalidSelector {
                        namespace: namespace.to_string(),
                        selector: selector.to_string(),
                    });
                }
                PermissionMatcher::WebFetch {
                    domain: domain.to_ascii_lowercase(),
                }
            }
            "Agent" => {
                let selector = non_blank_selector(namespace, selector)?;
                let profile = selector.strip_prefix("profile:").ok_or_else(|| {
                    PermissionRuleError::InvalidSelector {
                        namespace: namespace.to_string(),
                        selector: selector.to_string(),
                    }
                })?;
                PermissionMatcher::Agent {
                    profile: non_blank_selector(namespace, profile)?.to_string(),
                }
            }
            "ExactTool" => PermissionMatcher::ExactTool {
                tool: non_blank_selector(namespace, selector)?.to_string(),
            },
            other => return Err(PermissionRuleError::UnknownNamespace(other.to_string())),
        }
    };

    #[derive(Serialize)]
    struct CausePayload<'a> {
        effect: PolicyEffect,
        origin: &'a PolicyOrigin,
        matcher: &'a PermissionMatcher,
    }
    let cause_id = PermissionCauseId::derive(&CausePayload {
        effect,
        origin: &origin,
        matcher: &matcher,
    })?;
    Ok(PermissionRule {
        effect,
        explanation: SafeExplanation::new(format!("{origin:?} permission rule")),
        origin,
        matcher,
        cause_id,
    })
}

/// Invalid product permission rule.
#[derive(Debug, Error)]
pub enum PermissionRuleError {
    /// Empty rules never become broad matchers.
    #[error("permission rule is empty")]
    Empty,
    /// Rule matching remains bounded under untrusted tool input.
    #[error("permission rule exceeds the maximum of {maximum} characters")]
    TooLong {
        /// Trusted product limit.
        maximum: usize,
    },
    /// Rules other than `TodoWrite` and `mcp.*` require `Namespace(selector)`.
    #[error("permission rule `{0}` must use Namespace(selector) syntax")]
    InvalidSyntax(String),
    /// Namespace compiler is not registered.
    #[error("unknown permission namespace `{0}`")]
    UnknownNamespace(String),
    /// Namespace-specific selector validation failed.
    #[error("invalid {namespace} selector `{selector}`")]
    InvalidSelector {
        /// Namespace named by the rule.
        namespace: String,
        /// Rejected selector text.
        selector: String,
    },
    /// MCP shorthand must name exactly one server and tool.
    #[error("invalid MCP permission rule `{0}`")]
    InvalidMcp(String),
    /// Stable cause data could not be encoded.
    #[error("failed to encode permission rule: {0}")]
    Encoding(#[from] serde_json::Error),
}

fn parse_call_syntax(raw: &str) -> Result<(&str, &str), PermissionRuleError> {
    let open = raw
        .find('(')
        .ok_or_else(|| PermissionRuleError::InvalidSyntax(raw.to_string()))?;
    if !raw.ends_with(')') || raw[open + 1..raw.len() - 1].contains(')') {
        return Err(PermissionRuleError::InvalidSyntax(raw.to_string()));
    }
    let namespace = raw[..open].trim();
    if namespace.is_empty() {
        return Err(PermissionRuleError::InvalidSyntax(raw.to_string()));
    }
    Ok((namespace, raw[open + 1..raw.len() - 1].trim()))
}

fn compile_path_matcher(
    selector: &str,
    context: &RuleCompileContext,
) -> Result<PathMatcher, PermissionRuleError> {
    let selector = normalize_path_rule(non_blank_selector("path", selector)?)?;
    if selector.starts_with('/') {
        Ok(PathMatcher {
            anchor: None,
            pattern: selector,
        })
    } else {
        Ok(PathMatcher {
            anchor: Some(context.workspace_root.clone()),
            pattern: selector,
        })
    }
}

fn normalize_path_rule(selector: &str) -> Result<String, PermissionRuleError> {
    let selector = selector.replace('\\', "/");
    let absolute = selector.starts_with('/');
    let mut parts = Vec::new();
    for part in selector.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                return Err(PermissionRuleError::InvalidSelector {
                    namespace: "path".to_string(),
                    selector,
                });
            }
            part => parts.push(part),
        }
    }
    let normalized = parts.join("/");
    if normalized.is_empty() && !absolute {
        return Err(PermissionRuleError::InvalidSelector {
            namespace: "path".to_string(),
            selector,
        });
    }
    Ok(if absolute {
        format!("/{normalized}")
    } else {
        normalized
    })
}

fn non_blank_selector<'a>(
    namespace: &str,
    selector: &'a str,
) -> Result<&'a str, PermissionRuleError> {
    if selector.trim().is_empty() {
        Err(PermissionRuleError::InvalidSelector {
            namespace: namespace.to_string(),
            selector: selector.to_string(),
        })
    } else {
        Ok(selector)
    }
}

fn strip_path_prefix<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    if path == root {
        return Some("");
    }
    path.strip_prefix(root)?.strip_prefix('/')
}

fn contains_wildcard(pattern: &str) -> bool {
    pattern.contains(['*', '?'])
}

fn origin_domain(origin: &str) -> Option<&str> {
    let (_, authority) = origin.split_once("://")?;
    let authority = authority.split('/').next().unwrap_or(authority);
    let host = authority
        .strip_prefix('[')
        .and_then(|value| value.split_once(']').map(|(host, _)| host))
        .or_else(|| authority.split(':').next())?;
    (!host.is_empty()).then_some(host)
}

fn domain_matches(rule: &str, host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == rule
        || host
            .strip_suffix(rule)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

fn valid_dns_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 253
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
}

#[cfg(test)]
mod permission_rule_tests {
    use std::path::Path;

    use super::*;
    use crate::permission::{
        CanonicalCommand, CommandKind, PermissionCheckSpec, PermissionNamespace, ProfileDefault,
        ToolPermissionProfile,
    };

    fn context() -> RuleCompileContext {
        RuleCompileContext::new(
            CanonicalPath::from_absolute(Path::new("/workspace")).expect("absolute path"),
        )
    }

    fn check(target: PermissionTarget, namespace: PermissionNamespace) -> PermissionCheck {
        ToolPermissionProfile::new(
            "tool",
            [(namespace, ProfileDefault::RequireApproval)],
            false,
        )
        .expect("valid profile")
        .validate([PermissionCheckSpec::new(target)])
        .expect("valid check")
        .first()
        .clone()
    }

    #[test]
    fn relative_path_rules_are_anchored_to_the_workspace() {
        let rule = compile_permission_rule(
            "Read(./src/**)",
            PolicyEffect::Allow,
            PolicyOrigin::User,
            &context(),
        )
        .expect("valid rule");
        let inside = check(
            PermissionTarget::Read(PathSelector::exact(
                CanonicalPath::from_absolute(Path::new("/workspace/src/lib.rs"))
                    .expect("absolute path"),
            )),
            PermissionNamespace::Read,
        );
        let outside = check(
            PermissionTarget::Read(PathSelector::exact(
                CanonicalPath::from_absolute(Path::new("/other/src/lib.rs"))
                    .expect("absolute path"),
            )),
            PermissionNamespace::Read,
        );
        assert!(rule.contribution(&inside).is_some());
        assert!(rule.contribution(&outside).is_none());
    }

    #[test]
    fn opaque_commands_require_exact_allow_rules() {
        let broad = compile_permission_rule(
            "Bash(cargo *)",
            PolicyEffect::Allow,
            PolicyOrigin::User,
            &context(),
        )
        .expect("valid rule");
        let exact = compile_permission_rule(
            "Bash(cargo test)",
            PolicyEffect::Allow,
            PolicyOrigin::User,
            &context(),
        )
        .expect("valid rule");
        let command = check(
            PermissionTarget::Bash(
                CanonicalCommand::new("cargo test", CommandKind::Opaque).expect("valid command"),
            ),
            PermissionNamespace::Bash,
        );
        assert!(broad.contribution(&command).is_none());
        assert!(exact.contribution(&command).is_some());
    }

    #[test]
    fn mcp_rules_match_exact_server_and_tool() {
        let rule = compile_permission_rule(
            "mcp.github.create_issue",
            PolicyEffect::RequireApproval,
            PolicyOrigin::Managed,
            &context(),
        )
        .expect("valid rule");
        let target = PermissionTarget::Mcp(
            crate::permission::McpSelector::new("github", "create_issue", [])
                .expect("valid selector"),
        );
        let check = check(target, PermissionNamespace::Mcp);
        assert!(rule.contribution(&check).is_some());
    }

    #[test]
    fn old_bare_tool_syntax_is_rejected() {
        assert!(matches!(
            compile_permission_rule(
                "read_file",
                PolicyEffect::Allow,
                PolicyOrigin::Project,
                &context(),
            ),
            Err(PermissionRuleError::InvalidSyntax(_))
        ));
    }

    #[test]
    fn web_domain_rules_match_only_the_domain_and_its_subdomains() {
        let rule = compile_permission_rule(
            "WebFetch(domain:example.com)",
            PolicyEffect::Allow,
            PolicyOrigin::User,
            &context(),
        )
        .expect("valid domain rule");
        for origin in [
            "https://example.com/path",
            "https://sub.example.com:8443/path",
            "https://example.com./path",
        ] {
            let target = PermissionTarget::WebFetch(
                crate::permission::CanonicalOrigin::new(origin).expect("valid origin"),
            );
            assert!(
                rule.contribution(&check(target, PermissionNamespace::WebFetch))
                    .is_some(),
                "{origin}"
            );
        }
        let lookalike = PermissionTarget::WebFetch(
            crate::permission::CanonicalOrigin::new("https://badexample.com")
                .expect("valid origin"),
        );
        assert!(
            rule.contribution(&check(lookalike, PermissionNamespace::WebFetch))
                .is_none()
        );
    }

    #[test]
    fn web_domain_rules_reject_wildcards_and_ip_literals() {
        for selector in [
            "WebFetch(domain:*.example.com)",
            "WebFetch(domain:127.0.0.1)",
            "WebFetch(domain:bad..example.com)",
        ] {
            assert!(
                matches!(
                    compile_permission_rule(
                        selector,
                        PolicyEffect::Allow,
                        PolicyOrigin::User,
                        &context(),
                    ),
                    Err(PermissionRuleError::InvalidSelector { .. })
                ),
                "{selector}"
            );
        }
    }
}
