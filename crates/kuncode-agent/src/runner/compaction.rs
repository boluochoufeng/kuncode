//! Automatic compaction at the quiescent boundary before a model request.
//!
//! No tool call is running at this boundary, so one frozen provider-visible
//! envelope can measure the baseline, every candidate, and the final dispatch.

use std::{sync::Arc, time::Instant};

use async_trait::async_trait;
use kuncode_core::{
    completion::{CompletionModel, CompletionRequest, CompletionRequestBuilder},
    non_empty_vec::NonEmptyVec,
};
use tokio_util::sync::CancellationToken;

use crate::{
    compaction::{
        CompactionDependencies, CompactionError, CompactionOutcome, GroupTokenEstimator,
        budget::{BudgetLevel, ContextBudget, TokenEstimator},
        compact_context,
        protocol::{ProtocolGroup, flatten_groups},
        summary::{LlmContextSummarizer, SummarizerError},
    },
    error::AgentError,
    observer::EventKind,
    session::AgentSession,
};

use super::AgentRunner;

mod artifact_counter;
mod cancellable_summary;
mod events;
mod shadow;

use artifact_counter::RequestArtifactCounter;
use cancellable_summary::CancellableSummarizer;
use events::{
    elapsed_ms, failure_code, failure_event, failure_message, invalidates_persistence_authority,
    is_recoverable, pressure_reason,
};

impl<M> AgentRunner<M>
where
    M: CompletionModel,
{
    /// Builds the next provider request, compacting first when policy requires it.
    ///
    /// Soft-pressure failures may reuse the original request when the attempt
    /// produced no authority-invalidating durable outcome. Hard pressure and
    /// authority-invalidating failures abort the turn instead.
    ///
    /// # Errors
    /// Returns [`AgentError`] when request projection or initial budget estimation
    /// fails, cancellation occurs, or a non-recoverable compaction cannot complete.
    pub(super) async fn prepare_request(
        &self,
        session: &mut AgentSession,
        iteration: usize,
        cancel: &CancellationToken,
    ) -> Result<CompletionRequest, AgentError> {
        // Freeze system text, tools, request options, and request-only runtime
        // state once so token comparisons differ only by active context.
        let projector = self.freeze_request_projector(session)?;
        let original = projector.project_agent(session.messages())?;
        let Some(runtime) = self.config.compaction.as_ref() else {
            return Ok(original);
        };
        if runtime.policy.mode() == crate::compaction::budget::CompactionMode::Disabled {
            return Ok(original);
        }
        let before =
            ContextBudget::for_request(&runtime.policy, &original, self.token_estimator.as_ref())
                .await
                .map_err(|error| compaction_error(CompactionError::Budget(error)))?;
        if runtime.policy.mode() == crate::compaction::budget::CompactionMode::Shadow {
            let artifact_counter = RequestArtifactCounter::new(self.token_estimator.as_ref());
            match shadow::observe(
                session.messages(),
                &runtime.policy,
                before,
                self.group_estimator.as_ref(),
                &artifact_counter,
            )
            .await
            {
                Ok(report) => self.emit(
                    session,
                    Some(iteration),
                    EventKind::CompactionObserved {
                        before_tokens: before.current_input(),
                        projected_after_tokens: report.projected_after_tokens,
                        safe_prefix_groups: report.safe_prefix_groups,
                        artifact_shape_candidates: report.artifact_shape_candidates,
                        requires_summary: report.requires_summary,
                        precision: before.precision(),
                    },
                ),
                Err(_) => self.emit(
                    session,
                    Some(iteration),
                    EventKind::CompactionSkipped {
                        reason: "shadow_planning_failed".to_string(),
                        before_tokens: before.current_input(),
                        precision: before.precision(),
                    },
                ),
            }
            return Ok(original);
        }
        let level = before.level(&runtime.policy);
        if level == BudgetLevel::Normal {
            self.emit(
                session,
                Some(iteration),
                EventKind::CompactionSkipped {
                    reason: "below_soft_threshold".to_string(),
                    before_tokens: before.current_input(),
                    precision: before.precision(),
                },
            );
            return Ok(original);
        }
        let started = Instant::now();
        self.emit(
            session,
            Some(iteration),
            EventKind::CompactionStarted {
                reason: pressure_reason(level).to_string(),
                before_tokens: before.current_input(),
                precision: before.precision(),
            },
        );
        let Some(store) = self.session_store.as_deref() else {
            let error = CompactionError::NonDurableSession;
            self.emit(
                session,
                Some(iteration),
                failure_event(&error, level, before, started),
            );
            if level == BudgetLevel::Soft {
                // No durable operation started, so the missing store produced no
                // ambiguous outcome; fallback is still forbidden at hard pressure.
                self.emit(
                    session,
                    Some(iteration),
                    EventKind::Warning {
                        message: failure_message(&error),
                    },
                );
                return Ok(original);
            }
            return Err(compaction_error(error));
        };
        let summarizer =
            match LlmContextSummarizer::new(self.summary_model.clone(), runtime.summary_max_tokens)
            {
                Ok(summarizer) => summarizer,
                Err(error) => {
                    let error = CompactionError::Summary(error);
                    self.emit(
                        session,
                        Some(iteration),
                        failure_event(&error, level, before, started),
                    );
                    return Err(compaction_error(error));
                }
            };
        let artifact_counter = RequestArtifactCounter::new(self.token_estimator.as_ref());
        let summarizer = CancellableSummarizer::new(&summarizer, cancel);
        let result = compact_context(CompactionDependencies {
            config: &runtime.policy,
            measured_before: before,
            session,
            store,
            projector: &projector,
            estimator: self.token_estimator.as_ref(),
            group_estimator: self.group_estimator.as_ref(),
            artifact_counter: &artifact_counter,
            summarizer: &summarizer,
            summary_model: &runtime.model_id,
        })
        .await;
        // Ambiguous durable outcomes and provenance violations poison the
        // session before recovery is considered, preventing a stale fallback.
        if let Err(error) = &result
            && invalidates_persistence_authority(error)
        {
            session
                .mark_persistence_failed("compaction persistence authority is no longer trusted");
        }
        match result {
            Ok(CompactionOutcome::Compacted(report)) => {
                self.emit(
                    session,
                    Some(iteration),
                    EventKind::CompactionCompleted {
                        before_tokens: report.before.current_input(),
                        after_tokens: report.after.current_input(),
                        target_reached: report.target_reached,
                        passes: report
                            .passes
                            .iter()
                            .map(|pass| pass.as_str().to_string())
                            .collect(),
                        source_seq_start: report.source_start.get(),
                        source_seq_end: report.source_end.get(),
                        checkpoint_seq: report.checkpoint_seq.get(),
                        artifact_count: report.artifact_count,
                        summary_usage: report.summary_usage,
                        summary_latency_ms: report.summary_latency_ms,
                        latency_ms: elapsed_ms(started),
                    },
                );
                projector.project_agent(session.messages())
            }
            Ok(CompactionOutcome::Bypassed)
            | Ok(CompactionOutcome::Observed(_))
            | Ok(CompactionOutcome::NotNeeded(_)) => Ok(original),
            Err(CompactionError::Summary(SummarizerError::Cancelled)) => Err(AgentError::Cancelled),
            Err(error) if is_recoverable(&error, level) => {
                self.emit(
                    session,
                    Some(iteration),
                    failure_event(&error, level, before, started),
                );
                self.emit(
                    session,
                    Some(iteration),
                    EventKind::Warning {
                        message: failure_message(&error),
                    },
                );
                // `is_recoverable` excludes every authority-invalidating error.
                Ok(original)
            }
            Err(error) => {
                self.emit(
                    session,
                    Some(iteration),
                    failure_event(&error, level, before, started),
                );
                Err(compaction_error(error))
            }
        }
    }
}

fn compaction_error(error: CompactionError) -> AgentError {
    AgentError::Compaction {
        message: failure_code(&error).to_string(),
    }
}

pub(super) struct RequestGroupEstimator {
    estimator: Arc<dyn TokenEstimator>,
}

impl RequestGroupEstimator {
    pub(super) fn new(estimator: Arc<dyn TokenEstimator>) -> Self {
        Self { estimator }
    }
}

#[async_trait]
impl GroupTokenEstimator for RequestGroupEstimator {
    async fn estimate(&self, group: &ProtocolGroup) -> Result<u64, CompactionError> {
        let messages = flatten_groups(std::slice::from_ref(group));
        let request = CompletionRequestBuilder::from_messages(
            NonEmptyVec::try_from(messages).map_err(|_| CompactionError::NoSafeBoundary)?,
        )
        .build();
        Ok(self.estimator.estimate(&request).await?.tokens())
    }
}
