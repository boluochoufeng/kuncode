//! Authorization receipt issuance and single-use tool dispatch.

use std::time::Instant;

use kuncode_core::completion::CompletionModel;

use crate::{
    error::AgentError,
    observer::EventKind,
    permission::{
        AuthorizationEngine, AuthorizationOutcome, ExecutionOutcome, PendingAuthorizationCall,
        SessionPolicyOverlay,
    },
    session::AgentSession,
    tool::{ToolContext, ToolResultRetention},
};

use super::{AgentRunner, CallOutcome};

const MAX_AUTHORIZATION_RESTARTS: usize = 8;

impl<M> AgentRunner<M>
where
    M: CompletionModel,
{
    /// Runs the only legal prepare-authorize-execute path for one model call.
    pub(super) async fn authorized_call(
        &self,
        session: &mut AgentSession,
        iteration: usize,
        tool_call_id: &str,
        name: &str,
        arguments: serde_json::Value,
        context: &ToolContext,
    ) -> Result<CallOutcome, AgentError> {
        let mut overlay = std::mem::take(session.permissions_mut());
        let pending = PendingAuthorizationCall::new(tool_call_id, name, arguments);
        let result = self
            .authorized_call_with_overlay(session, &mut overlay, iteration, pending, context)
            .await;
        *session.permissions_mut() = overlay;
        result
    }

    async fn authorized_call_with_overlay(
        &self,
        session: &mut AgentSession,
        overlay: &mut SessionPolicyOverlay,
        iteration: usize,
        mut pending: PendingAuthorizationCall,
        context: &ToolContext,
    ) -> Result<CallOutcome, AgentError> {
        let engine = AuthorizationEngine::new(
            &self.registry,
            self.policy.as_ref(),
            self.hooks.as_ref(),
            self.approvals.as_ref(),
        );
        let tool_call_id = pending.call_id().to_string();
        let name = pending.tool_name().to_string();
        let started = Instant::now();
        let mut start_emitted = false;
        let mut restart_count = 0usize;

        loop {
            let authorization = engine
                .authorize_with_progress(
                    pending,
                    overlay,
                    session.messages(),
                    iteration,
                    &context.cancel,
                    |request| {
                        if !start_emitted {
                            self.emit(
                                session,
                                Some(iteration),
                                EventKind::ToolStart {
                                    tool_call_id: tool_call_id.clone(),
                                    tool: request.tool().as_str().to_string(),
                                    summary: request.display().summary().to_string(),
                                },
                            );
                            start_emitted = true;
                        }
                    },
                )
                .await
                .map_err(|error| match error {
                    crate::permission::AuthorizationError::ToolProfile(source) => {
                        AgentError::ToolRegistration {
                            name: name.clone(),
                            source,
                        }
                    }
                    source => AgentError::Authorization(source),
                })?;
            match authorization {
                AuthorizationOutcome::Cancelled => return Err(AgentError::Cancelled),
                AuthorizationOutcome::Rejected(rejected) => {
                    let (_, output) = rejected.into_parts();
                    tracing::debug!(
                        target: "kuncode::authorization",
                        iteration,
                        tool_call_id,
                        tool = name,
                        outcome = "rejected",
                        latency_ms = elapsed_ms(started),
                        "tool authorization completed",
                    );
                    return Ok(CallOutcome {
                        output,
                        executed: false,
                        retention: ToolResultRetention::Verbatim,
                    });
                }
                AuthorizationOutcome::Authorized(authorized) => {
                    match engine.execute(authorized, overlay, context).await {
                        Ok(ExecutionOutcome::Executed(executed)) => {
                            let (_, receipt, executed) = (*executed).into_parts();
                            let (output, retention) = executed.into_parts();
                            tracing::debug!(
                                target: "kuncode::authorization",
                                iteration,
                                tool_call_id,
                                tool = name,
                                generation = receipt.generation(),
                                rewrite_count = receipt.rewrite_count(),
                                request_fingerprint = receipt.request_fingerprint().as_str(),
                                outcome = "executed",
                                latency_ms = elapsed_ms(started),
                                "authorized tool call consumed",
                            );
                            return Ok(CallOutcome {
                                output,
                                executed: true,
                                retention,
                            });
                        }
                        Ok(ExecutionOutcome::Stale(stale)) => {
                            if restart_count >= MAX_AUTHORIZATION_RESTARTS {
                                return Ok(CallOutcome {
                                    output: crate::tool::ToolOutput::failure(
                                        "authorization_stale",
                                        "tool authorization could not reach a stable execution context",
                                    ),
                                    executed: false,
                                    retention: ToolResultRetention::Verbatim,
                                });
                            }
                            restart_count = restart_count.saturating_add(1);
                            pending = stale;
                        }
                        Ok(ExecutionOutcome::Cancelled) => return Err(AgentError::Cancelled),
                        Err(source) => {
                            return Err(AgentError::Tool {
                                name: name.clone(),
                                source,
                            });
                        }
                    }
                }
            }
        }
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
