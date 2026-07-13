use std::collections::BTreeMap;

use kuncode_core::{
    completion::{AssistantContent, Message, ToolResult, ToolResultContent, UserContent},
    non_empty_vec::NonEmptyVec,
};

use super::{
    audit::audit_journal,
    boundary::{ArtifactSpillError, ArtifactSpillInput},
    hash::sha256_hex,
    marker::{MarkerPayload, MarkerSource, build_marker_result},
    preview::canonical_artifact_preview,
    types::{
        ArtifactSpillFailure, ArtifactSpillOutcome, ArtifactSpillResult, ArtifactStore,
        ArtifactTokenCounter,
    },
};
use crate::{
    compaction::protocol::ProtocolGroup,
    session_store::{NewToolArtifact, SessionId},
    tool::ToolOutput,
};

const ARTIFACT_THRESHOLD_TOKENS: u64 = 8_192;

/// Spills eligible results after durable writes and never mutates `input`.
///
/// # Errors
/// Returns [`ArtifactSpillError`] when durable history cannot be audited or the
/// journal advances while an artifact is being committed.
pub async fn spill_artifacts(
    input: ArtifactSpillInput<'_>,
    store: &dyn ArtifactStore,
    counter: &dyn ArtifactTokenCounter,
) -> Result<ArtifactSpillResult, ArtifactSpillError> {
    audit_journal(&input, store).await?;
    let runtime = SpillRuntime {
        session: input.durable.session_id(),
        store,
        counter,
    };
    let mut pass = ArtifactSpillResult {
        groups: input.groups.to_vec(),
        frontier: input.durable.frontier(),
        outcomes: Vec::new(),
    };
    for group_index in 0..input.protected_start {
        runtime.spill_group(group_index, &mut pass).await?;
    }
    Ok(pass)
}

struct SpillRuntime<'a> {
    session: &'a SessionId,
    store: &'a dyn ArtifactStore,
    counter: &'a dyn ArtifactTokenCounter,
}

impl SpillRuntime<'_> {
    async fn spill_group(
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
        for message in &mut candidate_results {
            self.spill_result_message(message, &call_names, pass)
                .await?;
        }
        if let ProtocolGroup::ToolExchange { results, .. } = &mut pass.groups[group_index] {
            *results = candidate_results;
        }
        Ok(())
    }

    async fn spill_result_message(
        &self,
        message: &mut Message,
        call_names: &BTreeMap<String, String>,
        pass: &mut ArtifactSpillResult,
    ) -> Result<(), ArtifactSpillError> {
        let Message::User { content } = message else {
            return Ok(());
        };
        let mut candidate = content.clone().into_vec();
        for block in &mut candidate {
            let UserContent::ToolResult(result) = block else {
                continue;
            };
            let Some(tool_name) = call_names.get(result.id.as_str()).map(String::as_str) else {
                failed(
                    pass,
                    result,
                    ArtifactSpillFailure::Parse(
                        "tool result has no matching assistant tool name".to_string(),
                    ),
                );
                continue;
            };
            if let Some(marker) = self.spill_one(result, tool_name, pass).await? {
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
        pass: &mut ArtifactSpillResult,
    ) -> Result<Option<ToolResult>, ArtifactSpillError> {
        let tokens = match self.counter.count(result).await {
            Ok(tokens) => tokens,
            Err(error) => {
                return Ok(failed(
                    pass,
                    result,
                    ArtifactSpillFailure::Count(error.to_string()),
                ));
            }
        };
        if tokens <= ARTIFACT_THRESHOLD_TOKENS {
            pass.outcomes.push(ArtifactSpillOutcome::BelowThreshold {
                tool_call_id: result.id.clone(),
                tokens,
            });
            return Ok(None);
        }
        let payload = match payload_text(result) {
            Ok(payload) => payload,
            Err(failure) => return Ok(failed(pass, result, failure)),
        };
        let output = match serde_json::from_str::<ToolOutput>(payload) {
            Ok(output) => output,
            Err(error) => {
                return Ok(failed(
                    pass,
                    result,
                    ArtifactSpillFailure::Parse(error.to_string()),
                ));
            }
        };
        let content_hash = format!("sha256-{}", sha256_hex(payload.as_bytes()));
        let source = match MarkerSource::new(
            tool_name,
            result,
            MarkerPayload {
                output: &output,
                content_hash: &content_hash,
                text: payload,
                tokens,
            },
        ) {
            Ok(source) => source,
            Err(failure) => return Ok(failed(pass, result, failure)),
        };
        let marker = match build_marker_result(&source, self.counter).await {
            Ok(marker) => marker,
            Err(failure) => return Ok(failed(pass, result, failure)),
        };
        let artifact = match NewToolArtifact::inline(
            &content_hash,
            canonical_artifact_preview(payload),
            payload,
        ) {
            Ok(artifact) => artifact,
            Err(error) => {
                return Ok(failed(
                    pass,
                    result,
                    ArtifactSpillFailure::Store(error.to_string()),
                ));
            }
        };
        let receipt = match self.store.put(self.session, pass.frontier, artifact).await {
            Ok(receipt) => receipt,
            Err(crate::session_store::SessionStoreError::JournalHeadConflict {
                expected,
                actual,
            }) => {
                return Err(ArtifactSpillError::JournalHeadConflict { expected, actual });
            }
            Err(error) => {
                return Ok(failed(
                    pass,
                    result,
                    ArtifactSpillFailure::Store(error.to_string()),
                ));
            }
        };
        pass.frontier = pass.frontier.max(receipt.journal_seq());
        pass.outcomes.push(ArtifactSpillOutcome::Spilled {
            tool_call_id: result.id.clone(),
            artifact_id: receipt.reference().artifact_id().to_string(),
            journal_seq: receipt.journal_seq(),
            original_tokens: tokens,
        });
        Ok(Some(marker.result))
    }
}

fn payload_text(result: &ToolResult) -> Result<&str, ArtifactSpillFailure> {
    if result.content.len() != 1 {
        return Err(ArtifactSpillFailure::Parse(
            "tool result must contain exactly one JSON text block".to_string(),
        ));
    }
    match result.content.first() {
        ToolResultContent::Text(text) => Ok(text.text_ref()),
    }
}

fn failed(
    pass: &mut ArtifactSpillResult,
    result: &ToolResult,
    failure: ArtifactSpillFailure,
) -> Option<ToolResult> {
    pass.outcomes.push(ArtifactSpillOutcome::Failed {
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
            AssistantContent::Text(_) | AssistantContent::Reasoning(_) => None,
        }) {
            names.insert(call.id.clone(), call.function.name.clone());
        }
    }
    names
}
