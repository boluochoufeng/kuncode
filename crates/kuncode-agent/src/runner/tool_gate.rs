//! Permission, hook, cancellation, and dispatch gates for one tool call.

use kuncode_core::completion::CompletionModel;

use crate::{
    error::AgentError,
    hook::{PreToolCx, PreToolOutcome},
    observer::EventKind,
    permission::{Decision, PermissionGate, Prepared},
    session::AgentSession,
    tool::{ToolContext, ToolError, ToolErrorKind, ToolOutput, ToolResultRetention},
};

use super::{AgentRunner, CallOutcome, cancellation::cancellable};

impl<M> AgentRunner<M>
where
    M: CompletionModel,
{
    /// Gates a tool call, then dispatches. The gate races the approval prompt
    /// against cancellation; this method races execution.
    ///
    /// Returns a [`CallOutcome`]: the model-recoverable [`ToolOutput`] to record
    /// plus whether the tool actually ran. Unknown tools, bad arguments, and
    /// hook/permission denials carry `executed: false` (the loop feeds the
    /// output back). A user `Abort` or a cancelled token escalates to
    /// [`AgentError::Cancelled`], and a harness-level tool failure to
    /// [`AgentError::Tool`]; both unwind the whole turn (so neither is a
    /// `CallOutcome` and neither fires `PostToolUse`).
    ///
    /// Emits [`EventKind::ToolStart`] between the gate's two phases — once a
    /// [`PermissionRequest`](crate::permission::PermissionRequest) is in hand —
    /// so an unknown tool / bad arguments (rejected by `prepare`) get a `ToolEnd`
    /// with no preceding `ToolStart`. `PreToolUse` fires after that row opens and
    /// before the gate decides, so a hook denial mirrors a permission denial.
    pub(super) async fn gated_call(
        &self,
        session: &mut AgentSession,
        iteration: usize,
        tool_call_id: &str,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<CallOutcome, AgentError> {
        let gate = PermissionGate {
            policy: self.policy.as_ref(),
            approver: self.approver.as_ref(),
            registry: &self.registry,
        };

        // prepare: resolve + parse. An unknown tool / bad arguments never produce
        // a request, so they short-circuit here with no `ToolStart`.
        let (tool, arguments, request) = match gate.prepare(name, arguments, ctx) {
            Prepared::Ready {
                tool,
                args,
                request,
            } => (tool, args, request),
            Prepared::Rejected(output) => {
                return Ok(CallOutcome {
                    output,
                    executed: false,
                    retention: ToolResultRetention::Verbatim,
                });
            }
        };

        // The request confirms a real call and carries a human summary, so open
        // the tool's UI row now — before the (possibly blocking) approval inside
        // `decide`.
        self.emit(
            session,
            Some(iteration),
            EventKind::ToolStart {
                tool_call_id: tool_call_id.to_string(),
                tool: request.tool.clone(),
                summary: request.summary.clone(),
            },
        );

        // PreToolUse runs after the row opens and before the gate decides.
        // Tighten-only: a `Deny` short-circuits *without* consulting the
        // approver, taking the same path (and event shape) as a permission deny.
        // The cx borrows the request/args/transcript, so it is scoped to end
        // before `decide` takes `&mut` of the session.
        if !self.hooks.is_empty() {
            let pre = {
                let cx = PreToolCx {
                    request: &request,
                    args: &arguments,
                    messages: session.messages(),
                    iteration,
                };
                cancellable(&ctx.cancel, self.hooks.pre_tool_use(&cx)).await
            };
            match pre {
                None => return Err(AgentError::Cancelled),
                Some(PreToolOutcome::Proceed) => {}
                Some(PreToolOutcome::Deny { message }) => {
                    return Ok(CallOutcome {
                        output: ToolOutput::failure(ToolErrorKind::BlockedByHook, message),
                        executed: false,
                        retention: ToolResultRetention::Verbatim,
                    });
                }
            }
        }

        match gate.decide(&request, session.permissions_mut(), ctx).await {
            Decision::Deny(output) => Ok(CallOutcome {
                output,
                executed: false,
                retention: ToolResultRetention::Verbatim,
            }),
            Decision::Abort => Err(AgentError::Cancelled),
            // Execute, racing cancellation so a long tool can be interrupted.
            Decision::Allow => {
                match cancellable(&ctx.cancel, tool.call(arguments.clone(), ctx)).await {
                    None => Err(AgentError::Cancelled),
                    Some(Ok(output)) => {
                        let retention = tool.result_retention(&arguments, &output);
                        Ok(CallOutcome {
                            output,
                            executed: true,
                            retention,
                        })
                    }
                    // A tool that surfaces its own cancellation is still a
                    // turn-level interrupt. The harness no longer synthesizes this
                    // (a cancelled token loses the race to `None` above), so this is
                    // a defensive arm for a tool that returns it itself.
                    Some(Err(ToolError::Cancelled)) => Err(AgentError::Cancelled),
                    Some(Err(source)) => Err(AgentError::Tool {
                        name: name.to_string(),
                        source,
                    }),
                }
            }
        }
    }
}
