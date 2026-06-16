//! Permission gate: turns one model-issued tool call into a [`Decision`].
//!
//! The gate is the permission module's composed interface. It owns tool
//! resolution, argument parsing, rule evaluation, the approval handshake, grant
//! recording, the model-recoverable denial payloads, and the structured audit
//! log. The runner calls it before dispatch and acts on the [`Decision`] —
//! keeping `docs/s03/permission-system.md` §5's "gate in the runner, before
//! dispatch" while giving the decision a seam with its own test surface.
//!
//! Two phases, split exactly where the runner emits `ToolStart` (so an unknown
//! tool / bad arguments — which never produce a [`PermissionRequest`] — get no
//! `ToolStart`):
//!
//! - [`prepare`](PermissionGate::prepare) — sync: resolve the tool and parse
//!   arguments into a [`Prepared::Ready`] carrying the request, or a
//!   [`Prepared::Rejected`] model-recoverable output.
//! - [`decide`](PermissionGate::decide) — async: [`evaluate`] against policy +
//!   session state, consult the [`Approver`] on an `Ask` (racing cancellation),
//!   record any grant, and return the [`Decision`].
//!
//! [`PermissionGate`] is a borrowed view over the runner's policy / approver /
//! registry, so a test builds it from hand-made parts — no model or loop needed.

use std::sync::Arc;

use crate::{
    permission::{
        ApprovalOutcome, Approver, DenyReason, PermissionPolicy, PermissionRequest,
        PermissionSessionState, RuleOrigin, Verdict, evaluate,
    },
    registry::ToolRegistry,
    tool::{Tool, ToolContext, ToolOutput},
};

/// Borrowed view over the inputs one permission decision needs. Cheap to build
/// per call from the runner's fields, and constructible directly in tests.
pub struct PermissionGate<'a> {
    /// Static rules, read-only (built-in deny ∪ project ∪ CLI flags).
    pub policy: &'a PermissionPolicy,
    /// Side-effecting prompt, consulted only on an `Ask` verdict.
    pub approver: &'a dyn Approver,
    /// Tool lookup, for resolving the model's tool name.
    pub registry: &'a ToolRegistry,
}

/// Outcome of [`PermissionGate::prepare`].
pub enum Prepared {
    /// Tool resolved and arguments parsed. The runner emits `ToolStart` from
    /// `request`, then calls [`decide`](PermissionGate::decide); on
    /// [`Decision::Allow`] it dispatches `tool` with `args`.
    Ready {
        /// Resolved tool handle, shared with the gate's lookup.
        tool: Arc<dyn Tool>,
        /// The raw arguments, threaded through to dispatch (parsed again there —
        /// the accepted double-parse, see `docs/s03/permission-system.md` §6).
        args: serde_json::Value,
        /// The computed permission request.
        request: PermissionRequest,
    },
    /// An unknown tool or unparseable arguments — a model-recoverable failure
    /// that never reached a request, so the runner gives it no `ToolStart`.
    Rejected(ToolOutput),
}

/// The gate's resolved instruction to the runner, after any approval.
#[derive(Debug)]
pub enum Decision {
    /// Run the tool the runner holds from [`Prepared::Ready`].
    Allow,
    /// Blocked by a rule or denied at the prompt — feed this model-recoverable
    /// output back to the model.
    Deny(ToolOutput),
    /// The user interrupted (prompt `Abort` or a cancelled token); the runner
    /// escalates to [`AgentError::Cancelled`](crate::error::AgentError::Cancelled).
    Abort,
}

impl PermissionGate<'_> {
    /// Resolves the tool and computes its [`PermissionRequest`]. Synchronous and
    /// side-effect-free — it touches neither policy nor the filesystem.
    ///
    /// A parse failure short-circuits to `invalid_arguments`, so bad arguments
    /// never reach [`decide`](Self::decide) or the approver.
    pub fn prepare(&self, name: &str, args: serde_json::Value, ctx: &ToolContext) -> Prepared {
        let Some(tool) = self.registry.get(name) else {
            return Prepared::Rejected(ToolOutput::failure(
                "unknown_tool",
                format!("tool `{name}` is not registered"),
            ));
        };

        match tool.permission(&args, ctx) {
            Ok(request) => Prepared::Ready {
                tool,
                args,
                request,
            },
            Err(failure) => Prepared::Rejected(failure),
        }
    }

    /// Evaluates `request` against policy + session state, consulting the
    /// [`Approver`] on an `Ask` (racing the context's cancellation token), and
    /// records any session grant. Emits a structured audit event per branch.
    pub async fn decide(
        &self,
        request: &PermissionRequest,
        state: &mut PermissionSessionState,
        ctx: &ToolContext,
    ) -> Decision {
        let resource = request.resource.as_deref().unwrap_or("-");

        match evaluate(self.policy, state, request) {
            Verdict::Allow => {
                audit(request, resource, "allow", None);
                Decision::Allow
            }
            Verdict::Deny(reason) => {
                audit(request, resource, "deny", Some(&reason.rule));
                Decision::Deny(rule_denied_output(&reason))
            }
            Verdict::Ask => {
                // Escalate to the approver, racing cancellation: waiting on a
                // terminal prompt is a place the user may hit Ctrl-C.
                let outcome = tokio::select! {
                    outcome = self.approver.request(request) => outcome,
                    _ = ctx.cancel.cancelled() => ApprovalOutcome::Abort,
                };
                match outcome {
                    ApprovalOutcome::AllowOnce => {
                        audit(request, resource, "allow_once", None);
                        Decision::Allow
                    }
                    ApprovalOutcome::AllowAlways(rule) => {
                        audit(request, resource, "allow_always", Some(&rule.raw));
                        state.grant_allow(rule);
                        Decision::Allow
                    }
                    ApprovalOutcome::DenyOnce => {
                        audit(request, resource, "deny_once", None);
                        Decision::Deny(user_denied_output(false))
                    }
                    ApprovalOutcome::DenyAlways(rule) => {
                        audit(request, resource, "deny_always", Some(&rule.raw));
                        state.grant_deny(rule);
                        Decision::Deny(user_denied_output(true))
                    }
                    ApprovalOutcome::Abort => {
                        audit(request, resource, "abort", None);
                        Decision::Abort
                    }
                }
            }
        }
    }
}

/// Emits one structured permission audit event (§13). With no `tracing`
/// subscriber installed this is a no-op; the CLI installs one so `RUST_LOG`
/// surfaces decisions.
fn audit(request: &PermissionRequest, resource: &str, decision: &str, rule: Option<&str>) {
    tracing::info!(
        target: "kuncode::permission",
        tool = %request.tool,
        action = ?request.action,
        resource = %resource,
        decision = %decision,
        rule = rule.unwrap_or("-"),
        "permission decision",
    );
}

/// Builds the model-recoverable output for a request blocked by a rule
/// (built-in deny, project/CLI deny, or a session deny-grant). The message tells
/// the model not to retry — denial is a clear result, like a non-zero exit.
fn rule_denied_output(reason: &DenyReason) -> ToolOutput {
    ToolOutput::failure(
        "permission_denied",
        format!(
            "blocked by {} rule `{}`. Do not retry; choose a different approach or ask the user.",
            origin_label(reason.origin),
            reason.rule
        ),
    )
}

/// Builds the model-recoverable output for a request the user denied at a
/// prompt. `always` distinguishes a one-off "no" from a remembered deny-grant.
fn user_denied_output(always: bool) -> ToolOutput {
    let lead = if always {
        "The user denied this and will not be asked again for similar calls."
    } else {
        "The user denied this action."
    };
    ToolOutput::failure(
        "permission_denied",
        format!("{lead} Do not retry; choose a different approach or ask the user."),
    )
}

fn origin_label(origin: RuleOrigin) -> &'static str {
    match origin {
        RuleOrigin::Builtin => "built-in",
        RuleOrigin::ProjectSettings => "project",
        RuleOrigin::CliFlag => "command-line",
        RuleOrigin::SessionGrant => "session",
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::json;

    use super::{Decision, PermissionGate, Prepared};
    use crate::{
        permission::{
            ApprovalOutcome, Approver, PermissionAction, PermissionPolicy, PermissionRequest,
            PermissionSessionState, RuleOrigin, ScriptedApprover, parse_rule,
        },
        registry::ToolRegistry,
        tool::{ToolContext, ToolOutput, bash::Bash},
        workspace::Workspace,
    };

    async fn registry_with_bash() -> ToolRegistry {
        let workspace = Workspace::from_current_dir()
            .await
            .expect("current directory is a valid workspace");
        let mut registry = ToolRegistry::new();
        registry.register(Bash::new(workspace));
        registry
    }

    fn exec_request(command: &str) -> PermissionRequest {
        PermissionRequest::new(
            "bash",
            PermissionAction::Execute,
            Some(command.to_string()),
            format!("run `{command}`"),
        )
    }

    fn rejected(prepared: Prepared) -> ToolOutput {
        match prepared {
            Prepared::Rejected(output) => output,
            Prepared::Ready { .. } => panic!("expected Rejected, got Ready"),
        }
    }

    fn denied(decision: Decision) -> ToolOutput {
        match decision {
            Decision::Deny(output) => output,
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    fn error_kind(output: &ToolOutput) -> String {
        output.error.as_ref().expect("error payload").kind.clone()
    }

    /// An approver that never answers — only a cancelled token resolves the race.
    struct HangApprover;

    #[async_trait]
    impl Approver for HangApprover {
        async fn request(&self, _request: &PermissionRequest) -> ApprovalOutcome {
            std::future::pending().await
        }
    }

    /// An approver that must never be consulted; consulting it fails the test.
    struct NeverApprover;

    #[async_trait]
    impl Approver for NeverApprover {
        async fn request(&self, _request: &PermissionRequest) -> ApprovalOutcome {
            panic!("the approver must not be consulted");
        }
    }

    // ---- prepare ----

    #[tokio::test]
    async fn prepare_rejects_unknown_tool() {
        let registry = ToolRegistry::new();
        let policy = PermissionPolicy::builtin();
        let gate = PermissionGate {
            policy: &policy,
            approver: &NeverApprover,
            registry: &registry,
        };

        let output = rejected(gate.prepare("ghost", json!({}), &ToolContext::new()));
        assert_eq!(error_kind(&output), "unknown_tool");
    }

    #[tokio::test]
    async fn prepare_rejects_bad_arguments_before_any_prompt() {
        let registry = registry_with_bash().await;
        let policy = PermissionPolicy::builtin();
        let gate = PermissionGate {
            policy: &policy,
            approver: &NeverApprover,
            registry: &registry,
        };

        // `bash` needs a `cmd` string; missing it is a parse failure that must
        // short-circuit to `invalid_arguments`, never reaching the approver.
        let output = rejected(gate.prepare("bash", json!({}), &ToolContext::new()));
        assert_eq!(error_kind(&output), "invalid_arguments");
    }

    #[tokio::test]
    async fn prepare_ready_carries_the_request() {
        let registry = registry_with_bash().await;
        let policy = PermissionPolicy::builtin();
        let gate = PermissionGate {
            policy: &policy,
            approver: &NeverApprover,
            registry: &registry,
        };

        let Prepared::Ready { request, .. } =
            gate.prepare("bash", json!({ "cmd": "ls -la" }), &ToolContext::new())
        else {
            panic!("a known tool with valid arguments should be Ready");
        };
        assert_eq!(request.tool, "bash");
        assert_eq!(request.action, PermissionAction::Execute);
    }

    // ---- decide ----

    #[tokio::test]
    async fn decide_denies_on_a_deny_rule() {
        let registry = registry_with_bash().await;
        let mut policy = PermissionPolicy::new();
        policy
            .deny
            .extend(parse_rule("Bash(curl*)", RuleOrigin::ProjectSettings).unwrap());
        let gate = PermissionGate {
            policy: &policy,
            approver: &NeverApprover, // a deny rule wins before any prompt
            registry: &registry,
        };
        let mut state = PermissionSessionState::default();

        let output = denied(
            gate.decide(
                &exec_request("curl http://evil.test"),
                &mut state,
                &ToolContext::new(),
            )
            .await,
        );
        assert_eq!(error_kind(&output), "permission_denied");
        let message = output.error.expect("error payload").message;
        assert!(message.contains("Bash(curl*)"), "got {message}");
    }

    #[tokio::test]
    async fn decide_allows_reads_without_consulting_the_approver() {
        let registry = registry_with_bash().await;
        let policy = PermissionPolicy::builtin();
        let gate = PermissionGate {
            policy: &policy,
            approver: &NeverApprover, // Read defaults to Allow — no prompt
            registry: &registry,
        };
        let mut state = PermissionSessionState::default();

        let request = PermissionRequest::new(
            "read_file",
            PermissionAction::Read,
            Some("src/lib.rs".to_string()),
            "read",
        );
        assert!(matches!(
            gate.decide(&request, &mut state, &ToolContext::new()).await,
            Decision::Allow
        ));
    }

    #[tokio::test]
    async fn decide_denies_once_at_the_prompt() {
        let registry = registry_with_bash().await;
        let policy = PermissionPolicy::builtin();
        let approver = ScriptedApprover::new([ApprovalOutcome::DenyOnce]);
        let gate = PermissionGate {
            policy: &policy,
            approver: &approver,
            registry: &registry,
        };
        let mut state = PermissionSessionState::default();

        // Execute defaults to Ask; the user says no this once.
        let output = denied(
            gate.decide(
                &exec_request("rm notes.txt"),
                &mut state,
                &ToolContext::new(),
            )
            .await,
        );
        assert_eq!(error_kind(&output), "permission_denied");
    }

    #[tokio::test]
    async fn decide_allow_always_records_a_grant_and_skips_the_second_prompt() {
        let registry = registry_with_bash().await;
        let policy = PermissionPolicy::builtin();
        let grant = parse_rule("Bash(printf*)", RuleOrigin::SessionGrant).unwrap()[0].clone();
        // Exactly ONE scripted outcome: a second prompt would panic ("ran out").
        let approver = ScriptedApprover::new([ApprovalOutcome::AllowAlways(grant)]);
        let gate = PermissionGate {
            policy: &policy,
            approver: &approver,
            registry: &registry,
        };
        let mut state = PermissionSessionState::default();
        let ctx = ToolContext::new();

        assert!(matches!(
            gate.decide(&exec_request("printf one"), &mut state, &ctx)
                .await,
            Decision::Allow
        ));
        assert_eq!(state.allow_grants().len(), 1);
        // The grant now short-circuits the gate — no second approval consulted.
        assert!(matches!(
            gate.decide(&exec_request("printf two"), &mut state, &ctx)
                .await,
            Decision::Allow
        ));
    }

    #[tokio::test]
    async fn decide_deny_always_records_a_grant() {
        let registry = registry_with_bash().await;
        let policy = PermissionPolicy::builtin();
        let grant = parse_rule("Bash(rm*)", RuleOrigin::SessionGrant).unwrap()[0].clone();
        let approver = ScriptedApprover::new([ApprovalOutcome::DenyAlways(grant)]);
        let gate = PermissionGate {
            policy: &policy,
            approver: &approver,
            registry: &registry,
        };
        let mut state = PermissionSessionState::default();

        let output = denied(
            gate.decide(
                &exec_request("rm notes.txt"),
                &mut state,
                &ToolContext::new(),
            )
            .await,
        );
        assert_eq!(error_kind(&output), "permission_denied");
        assert_eq!(state.deny_grants().len(), 1);
    }

    #[tokio::test]
    async fn decide_aborts_on_prompt_abort() {
        let registry = registry_with_bash().await;
        let policy = PermissionPolicy::builtin();
        let approver = ScriptedApprover::new([ApprovalOutcome::Abort]);
        let gate = PermissionGate {
            policy: &policy,
            approver: &approver,
            registry: &registry,
        };
        let mut state = PermissionSessionState::default();

        assert!(matches!(
            gate.decide(&exec_request("printf hi"), &mut state, &ToolContext::new())
                .await,
            Decision::Abort
        ));
    }

    #[tokio::test]
    async fn decide_aborts_when_the_token_is_cancelled_during_approval() {
        let registry = registry_with_bash().await;
        let policy = PermissionPolicy::builtin();
        let gate = PermissionGate {
            policy: &policy,
            approver: &HangApprover, // never answers; only the token can resolve
            registry: &registry,
        };
        let mut state = PermissionSessionState::default();

        // A pre-cancelled token makes the cancel branch the only ready one, so
        // the approval race lands on Abort deterministically.
        let ctx = ToolContext::new();
        ctx.cancel.cancel();
        assert!(matches!(
            gate.decide(&exec_request("printf hi"), &mut state, &ctx)
                .await,
            Decision::Abort
        ));
    }
}
