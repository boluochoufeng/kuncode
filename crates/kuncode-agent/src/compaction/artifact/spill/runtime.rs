//! Applies audited lineage and spill policy to candidate protocol groups.
//!
//! Runtime coordinates provider-visible token decisions with durable source
//! positions while keeping the original groups untouched on isolated failures.

use std::collections::BTreeMap;

use kuncode_core::{
    completion::{AssistantContent, Message, ToolResult, UserContent},
    non_empty_vec::NonEmptyVec,
};

use super::super::{
    audit::JournalAudit,
    boundary::ArtifactSpillError,
    hash::tool_result_hash,
    types::{
        ArtifactResultLocation, ArtifactSpillFailure, ArtifactSpillOutcome, ArtifactSpillResult,
        ArtifactStore, ArtifactTokenCounter, BelowThresholdArtifact,
    },
};
use crate::{
    compaction::protocol::ProtocolGroup,
    session_store::{Seq, SessionId},
    tool::ToolResultRetention,
};

mod persist;

use persist::spill_large_result;

const ARTIFACT_THRESHOLD_TOKENS: u64 = 8_192;

pub(super) struct SpillRuntime<'a> {
    session: &'a SessionId,
    store: &'a dyn ArtifactStore,
    counter: &'a dyn ArtifactTokenCounter,
    audit: &'a JournalAudit,
    group_message_starts: &'a [usize],
}

impl<'a> SpillRuntime<'a> {
    pub(super) fn new(
        session: &'a SessionId,
        store: &'a dyn ArtifactStore,
        counter: &'a dyn ArtifactTokenCounter,
        audit: &'a JournalAudit,
        group_message_starts: &'a [usize],
    ) -> Self {
        Self {
            session,
            store,
            counter,
            audit,
            group_message_starts,
        }
    }

    pub(super) async fn spill_group(
        &self,
        group_index: usize,
        pass: &mut ArtifactSpillResult,
    ) -> Result<(), ArtifactSpillError> {
        let (call_names, mut candidate_results) = match &pass.groups[group_index] {
            ProtocolGroup::ToolExchange { assistant, results } => {
                (call_names(assistant), results.clone())
            }
            ProtocolGroup::Message(_) => return Ok(()),
        };
        for (result_message_index, message) in candidate_results.iter_mut().enumerate() {
            self.spill_result_message(
                group_index,
                result_message_index,
                message,
                &call_names,
                pass,
            )
            .await?;
        }
        if let ProtocolGroup::ToolExchange { results, .. } = &mut pass.groups[group_index] {
            *results = candidate_results;
        }
        Ok(())
    }

    async fn spill_result_message(
        &self,
        group_index: usize,
        result_message_index: usize,
        message: &mut Message,
        call_names: &BTreeMap<String, String>,
        pass: &mut ArtifactSpillResult,
    ) -> Result<(), ArtifactSpillError> {
        let Message::User { content } = message else {
            return Ok(());
        };
        // Work on a detached message so an isolated count, parse, or store
        // failure leaves the exact active payload available to later passes.
        let mut candidate = content.clone().into_vec();
        for (content_index, block) in candidate.iter_mut().enumerate() {
            let UserContent::ToolResult(result) = block else {
                continue;
            };
            let location = ArtifactResultLocation {
                group_index,
                result_message_index,
                content_index,
            };
            // Only results with exact verbatim journal lineage may cross the
            // lossy artifact boundary; derived context remains inline.
            let Some(source_journal_seq) = self.result_message_seq(location)? else {
                continue;
            };
            let retention = self.result_message_retention(location)?;
            let Some(tool_name) = call_names.get(result.id.as_str()).map(String::as_str) else {
                failed(
                    pass,
                    result,
                    location,
                    ArtifactSpillFailure::Parse(
                        "tool result has no matching assistant tool name".to_string(),
                    ),
                );
                continue;
            };
            if let Some(marker) = self
                .spill_one(
                    result,
                    tool_name,
                    location,
                    source_journal_seq,
                    retention,
                    pass,
                )
                .await?
            {
                *result = marker;
            }
        }
        if let Some((first, rest)) = candidate.split_first() {
            *content = NonEmptyVec::from_first_rest(first.clone(), rest.to_vec());
        }
        Ok(())
    }

    async fn spill_one(
        &self,
        result: &ToolResult,
        tool_name: &str,
        location: ArtifactResultLocation,
        source_journal_seq: Seq,
        retention: ToolResultRetention,
        pass: &mut ArtifactSpillResult,
    ) -> Result<Option<ToolResult>, ArtifactSpillError> {
        let tokens = match self.counter.count(result).await {
            Ok(tokens) => tokens,
            Err(error) => {
                return Ok(failed(
                    pass,
                    result,
                    location,
                    ArtifactSpillFailure::Count(error.to_string()),
                ));
            }
        };
        if tokens <= ARTIFACT_THRESHOLD_TOKENS {
            let source_hash = match tool_result_hash(result) {
                Ok(source_hash) => source_hash,
                Err(error) => {
                    return Ok(failed(
                        pass,
                        result,
                        location,
                        ArtifactSpillFailure::Parse(error.to_string()),
                    ));
                }
            };
            pass.outcomes.push(ArtifactSpillOutcome::BelowThreshold(
                BelowThresholdArtifact::new(
                    location,
                    result.id.clone(),
                    tokens,
                    source_hash,
                    Some(source_journal_seq),
                    retention,
                ),
            ));
            return Ok(None);
        }
        spill_large_result(self, result, tool_name, location, tokens, pass).await
    }

    fn result_message_seq(
        &self,
        location: ArtifactResultLocation,
    ) -> Result<Option<Seq>, ArtifactSpillError> {
        let Some(message_index) = self
            .group_message_starts
            .get(location.group_index)
            .and_then(|start| start.checked_add(1 + location.result_message_index))
        else {
            return Err(ArtifactSpillError::InvalidLineage);
        };
        Ok(self.audit.message_seq(message_index))
    }

    fn result_message_retention(
        &self,
        location: ArtifactResultLocation,
    ) -> Result<ToolResultRetention, ArtifactSpillError> {
        let Some(message_index) = self
            .group_message_starts
            .get(location.group_index)
            .and_then(|start| start.checked_add(1 + location.result_message_index))
        else {
            return Err(ArtifactSpillError::InvalidLineage);
        };
        Ok(self.audit.message_retention(message_index))
    }
}

pub(super) fn group_message_starts(groups: &[ProtocolGroup]) -> Vec<usize> {
    // Protocol-group coordinates differ from flattened lineage coordinates;
    // recording each start keeps receipt decisions attached to the audited row.
    let mut next = 0;
    groups
        .iter()
        .map(|group| {
            let start = next;
            next += match group {
                ProtocolGroup::Message(_) => 1,
                ProtocolGroup::ToolExchange { results, .. } => results.len() + 1,
            };
            start
        })
        .collect()
}

fn failed(
    pass: &mut ArtifactSpillResult,
    result: &ToolResult,
    location: ArtifactResultLocation,
    failure: ArtifactSpillFailure,
) -> Option<ToolResult> {
    pass.outcomes.push(ArtifactSpillOutcome::Failed {
        location,
        tool_call_id: result.id.clone(),
        failure,
    });
    None
}

fn call_names(message: &Message) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    if let Message::Assistant { content, .. } = message {
        for call in content.iter().filter_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call),
            AssistantContent::Text(_)
            | AssistantContent::Reasoning(_)
            | AssistantContent::Refusal(_) => None,
        }) {
            names.insert(call.id.clone(), call.function.name.clone());
        }
    }
    names
}
