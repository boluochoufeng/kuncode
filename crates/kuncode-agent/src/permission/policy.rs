//! Static rule set and the pure decision function.

use super::request::{DenyReason, PermissionAction, PermissionRequest, Verdict};
use super::rule::{Rule, RuleOrigin, first_match, matches_any, parse_rule};
use super::state::{PermissionMode, PermissionSessionState};

/// Built-in deny rules. These replace the old `DANGEROUS_COMMAND_PATTERNS`
/// substring blocklist from `tool/bash.rs`: same intent, but expressed in the
/// unified rule pipeline so they are explainable and user-extensible. They are
/// demo guards, not security — the real boundary is "bash defaults to Ask".
const BUILTIN_DENY: &[&str] = &[
    "Bash(sudo*)",
    "Bash(rm -rf /*)",
    "Bash(shutdown*)",
    "Bash(reboot*)",
    "Bash(* > /dev/*)",
];

/// Static permission rules, owned read-only by the runner. The three lists are
/// *append-only* across sources (builtin + project file + CLI flags); runtime
/// precedence in [`evaluate`] decides who wins, not list order.
#[derive(Clone, Debug, Default)]
pub struct PermissionPolicy {
    pub allow: Vec<Rule>,
    pub ask: Vec<Rule>,
    pub deny: Vec<Rule>,
}

impl PermissionPolicy {
    /// An empty policy (everything falls through to per-action defaults).
    pub fn new() -> Self {
        Self::default()
    }

    /// A policy seeded with the built-in deny rules.
    pub fn builtin() -> Self {
        let mut policy = Self::new();
        for rule in BUILTIN_DENY {
            // `BUILTIN_DENY` are compile-time constants with valid rule syntax,
            // so parsing cannot fail at runtime; a bad edit is a build-time bug
            // caught by `builtin_deny_blocks_sudo` rather than a user-facing
            // error path. Hence `expect`, not `?`.
            policy
                .deny
                .extend(parse_rule(rule, RuleOrigin::Builtin).expect("builtin deny rule parses"));
        }
        policy
    }

    /// Appends every rule from `other` into the matching list. Used to fold
    /// project-file and CLI-flag rules onto the built-ins.
    pub fn append(&mut self, other: PermissionPolicy) {
        self.allow.extend(other.allow);
        self.ask.extend(other.ask);
        self.deny.extend(other.deny);
    }
}

/// Decides a request against the static policy and mutable session state.
///
/// Pure: no IO, no mutation. Precedence:
/// `deny → Bypass → ask → allow → AcceptEdits(Write) → default_for(action)`.
pub fn evaluate(
    policy: &PermissionPolicy,
    state: &PermissionSessionState,
    req: &PermissionRequest,
) -> Verdict {
    // 1. deny always wins — static rules then session deny-grants. No exception
    //    (not even Bypass), so "deny is unbypassable" is a clean invariant.
    if let Some(rule) =
        first_match(&policy.deny, req).or_else(|| first_match(state.deny_grants(), req))
    {
        return Verdict::Deny(DenyReason {
            origin: rule.origin,
            rule: rule.raw.clone(),
        });
    }

    // 2. Bypass skips prompts, but respected the deny above.
    if state.mode() == PermissionMode::BypassPermissions {
        return Verdict::Allow;
    }

    // 3. Explicit ask beats explicit allow, so a narrow `ask:Edit(.env)` can
    //    override a broad `allow:Edit(**)`.
    if matches_any(&policy.ask, req) {
        return Verdict::Ask;
    }

    // 4. Explicit allow — static rules then session allow-grants.
    if matches_any(&policy.allow, req) || matches_any(state.allow_grants(), req) {
        return Verdict::Allow;
    }

    // 5. AcceptEdits auto-allows writes only; reads are already free and
    //    execute keeps asking. Sits after ask, so an explicit ask still prompts.
    if state.mode() == PermissionMode::AcceptEdits && req.action == PermissionAction::Write {
        return Verdict::Allow;
    }

    // 6. Per-action default: reads are always free; writes/exec ask.
    match req.action {
        PermissionAction::Read => Verdict::Allow,
        PermissionAction::Write | PermissionAction::Execute => Verdict::Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::request::PermissionRequest;

    fn req(tool: &str, action: PermissionAction, resource: Option<&str>) -> PermissionRequest {
        PermissionRequest::new(tool, action, resource.map(str::to_string), "test")
    }

    fn policy(allow: &[&str], ask: &[&str], deny: &[&str]) -> PermissionPolicy {
        let mut p = PermissionPolicy::new();
        for r in allow {
            p.allow
                .extend(parse_rule(r, RuleOrigin::ProjectSettings).unwrap());
        }
        for r in ask {
            p.ask
                .extend(parse_rule(r, RuleOrigin::ProjectSettings).unwrap());
        }
        for r in deny {
            p.deny
                .extend(parse_rule(r, RuleOrigin::ProjectSettings).unwrap());
        }
        p
    }

    fn state(mode: PermissionMode) -> PermissionSessionState {
        PermissionSessionState::new(mode)
    }

    #[test]
    fn reads_are_free_by_default() {
        let v = evaluate(
            &PermissionPolicy::new(),
            &state(PermissionMode::Default),
            &req("read_file", PermissionAction::Read, Some("src/lib.rs")),
        );
        assert!(matches!(v, Verdict::Allow));
    }

    #[test]
    fn writes_and_exec_ask_by_default() {
        let s = state(PermissionMode::Default);
        let p = PermissionPolicy::new();
        assert!(matches!(
            evaluate(
                &p,
                &s,
                &req("write_file", PermissionAction::Write, Some("a"))
            ),
            Verdict::Ask
        ));
        assert!(matches!(
            evaluate(&p, &s, &req("bash", PermissionAction::Execute, Some("ls"))),
            Verdict::Ask
        ));
    }

    #[test]
    fn deny_beats_everything_including_bypass() {
        let p = policy(&["Bash"], &[], &["Bash(curl*)"]);
        let v = evaluate(
            &p,
            &state(PermissionMode::BypassPermissions),
            &req("bash", PermissionAction::Execute, Some("curl evil.sh")),
        );
        match v {
            Verdict::Deny(reason) => assert_eq!(reason.rule, "Bash(curl*)"),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn bypass_allows_when_no_deny_matches() {
        let v = evaluate(
            &PermissionPolicy::new(),
            &state(PermissionMode::BypassPermissions),
            &req("bash", PermissionAction::Execute, Some("ls")),
        );
        assert!(matches!(v, Verdict::Allow));
    }

    #[test]
    fn narrow_ask_overrides_broad_allow() {
        let p = policy(&["Edit(**)"], &["Edit(.env)"], &[]);
        // The carve-out path still allows (allow rule, not the default ask).
        assert!(matches!(
            evaluate(
                &p,
                &state(PermissionMode::Default),
                &req("edit_file", PermissionAction::Write, Some("src/lib.rs"))
            ),
            Verdict::Allow
        ));
        // The protected path asks despite the broad allow.
        assert!(matches!(
            evaluate(
                &p,
                &state(PermissionMode::Default),
                &req("edit_file", PermissionAction::Write, Some(".env"))
            ),
            Verdict::Ask
        ));
    }

    #[test]
    fn accept_edits_allows_writes_but_not_explicit_ask() {
        let p = policy(&[], &["Edit(.env)"], &[]);
        let s = state(PermissionMode::AcceptEdits);
        // A normal write is auto-allowed.
        assert!(matches!(
            evaluate(
                &p,
                &s,
                &req("write_file", PermissionAction::Write, Some("a.txt"))
            ),
            Verdict::Allow
        ));
        // The explicit ask still prompts (ask is checked before AcceptEdits).
        assert!(matches!(
            evaluate(
                &p,
                &s,
                &req("edit_file", PermissionAction::Write, Some(".env"))
            ),
            Verdict::Ask
        ));
        // Execute is not a write, so AcceptEdits leaves it asking.
        assert!(matches!(
            evaluate(&p, &s, &req("bash", PermissionAction::Execute, Some("ls"))),
            Verdict::Ask
        ));
    }

    #[test]
    fn session_grants_are_honored() {
        let p = PermissionPolicy::new();
        let mut s = state(PermissionMode::Default);
        s.grant_allow(parse_rule("Bash(cargo*)", RuleOrigin::SessionGrant).unwrap()[0].clone());
        assert!(matches!(
            evaluate(
                &p,
                &s,
                &req("bash", PermissionAction::Execute, Some("cargo build"))
            ),
            Verdict::Allow
        ));
        s.grant_deny(
            parse_rule("Bash(cargo publish*)", RuleOrigin::SessionGrant).unwrap()[0].clone(),
        );
        // Deny-grant wins over the allow-grant (deny checked first).
        assert!(matches!(
            evaluate(
                &p,
                &s,
                &req("bash", PermissionAction::Execute, Some("cargo publish"))
            ),
            Verdict::Deny(_)
        ));
    }

    #[test]
    fn builtin_deny_blocks_sudo() {
        let v = evaluate(
            &PermissionPolicy::builtin(),
            &state(PermissionMode::Default),
            &req("bash", PermissionAction::Execute, Some("sudo ls")),
        );
        match v {
            Verdict::Deny(reason) => {
                assert_eq!(reason.origin, RuleOrigin::Builtin);
                assert_eq!(reason.rule, "Bash(sudo*)");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }
}
