//! Reconstructs an [`AgentSession`] from its durable checkpoint and journal.
//!
//! Resume re-derives every in-memory claim from durable evidence instead of
//! trusting the checkpoint alone: the summary head must reproduce from the
//! stored summary provenance, and every other active message must be explained
//! by a journal fact — byte-for-byte, or through the stable tool-result
//! identity for positions rewritten by deterministic compaction. What cannot
//! be proven fails resume rather than becoming fabricated lineage.
//!
//! Two properties are intentionally not reconstructed. Human authorship is
//! never recorded in the journal, so every resumed message carries
//! `human_authored = false`; the next live prompt re-establishes the anchor
//! boundary. Tool-result retention is only granted at the live tool boundary,
//! so resumed results stay [`Verbatim`](crate::tool::ToolResultRetention) and
//! are simply never re-slimmed.

use std::collections::BTreeSet;

use kuncode_core::completion::{Message, ToolResult, Usage, UserContent};

use super::{ActiveSummary, AgentSession, MessageCoverage, MessageLineage};
use crate::{
    compaction::{
        artifact::tool_result_hash,
        summary::{ContinuitySummary, project_summary_message},
    },
    permission::PermissionMode,
    session_store::{Checkpoint, JournalKind, Seq, SessionId, SessionStore, SessionStoreError},
};

impl AgentSession {
    /// Rebuilds the active context of an existing durable session.
    ///
    /// The latest checkpoint supplies the compacted base and the journal
    /// supplies everything after it; the durable frontier resumes at the
    /// journal head, so the session continues appending exactly where the
    /// previous process stopped. The permission overlay, todo plan, and read
    /// ledger start fresh: none of them are persisted, and an empty read
    /// ledger correctly forces re-reading before whole-file overwrites.
    ///
    /// # Errors
    /// Returns [`SessionResumeError`] when storage fails or when the stored
    /// checkpoint cannot be fully explained by journal facts. Resume never
    /// installs a partially proven context.
    pub async fn resume_durable_session(
        store: &dyn SessionStore,
        id: SessionId,
        mode: PermissionMode,
    ) -> Result<Self, SessionResumeError> {
        let checkpoint = store.latest_checkpoint(&id).await?;
        let entries = store.replay_after(&id, Seq::ZERO).await?;

        // `replay_after` returns ascending sequences, so the journal head is
        // the last entry of any kind — message or not.
        let head = entries.last().map_or(Seq::ZERO, |entry| entry.seq);
        let mut facts = Vec::new();
        let mut committed_artifacts = BTreeSet::new();
        for entry in entries {
            if entry.kind == JournalKind::Message.as_str() {
                let seq = entry.seq;
                facts.push((seq, entry.into_message()?));
            } else if entry.kind == JournalKind::ToolArtifact.as_str()
                && let Some(artifact_id) = entry
                    .payload_json
                    .get("artifact_id")
                    .and_then(serde_json::Value::as_str)
            {
                committed_artifacts.insert(artifact_id.to_string());
            }
        }

        let covers = checkpoint
            .as_ref()
            .map_or(Seq::ZERO, |checkpoint| checkpoint.covers_through_seq);
        let mut messages = Vec::new();
        let mut lineage = Vec::new();
        let mut active_summary = None;
        let mut cursor = 0;

        if let Some(checkpoint) = checkpoint {
            let mut base = restore_checkpoint_base(checkpoint)?;
            active_summary = base.summary.take();
            messages = base.messages;
            lineage = base.lineage;
            for (index, message) in base.unproven.into_iter().enumerate() {
                let proof =
                    match_journal_fact(&facts, &mut cursor, covers, &message, &committed_artifacts)
                        .ok_or(SessionResumeError::UnmatchedCheckpointMessage { index })?;
                messages.push(message);
                lineage.push(proof);
            }
        }

        // Everything after the checkpoint is live history appended by ordinary
        // turns; facts at or below `covers` that no active message claimed were
        // absorbed by compaction and stay journal-only.
        for (seq, message) in facts.drain(..) {
            if seq <= covers {
                continue;
            }
            messages.push(message);
            lineage.push(MessageLineage::appended(Some(seq), false));
        }

        let mut session = Self::with_mode(mode);
        session.messages = messages;
        session.message_lineage = lineage;
        session.active_summary = active_summary;
        session.session_id = Some(id);
        session.last_durable_seq = Some(head);
        Ok(session)
    }
}

/// Checkpoint state split into the provenance-proven summary head and the
/// remaining messages that still need journal proof.
struct CheckpointBase {
    messages: Vec<Message>,
    lineage: Vec<MessageLineage>,
    summary: Option<ActiveSummary>,
    unproven: Vec<Message>,
}

/// Rebinds the checkpoint's summary provenance to its projected head message.
///
/// A checkpoint with summary provenance always has that summary's projection
/// as its first active message: the semantic pass installs it at position
/// zero and deterministic passes never rewrite user-text positions. Resume
/// verifies the projection byte-for-byte instead of assuming it.
fn restore_checkpoint_base(checkpoint: Checkpoint) -> Result<CheckpointBase, SessionResumeError> {
    let Checkpoint {
        active_messages,
        summary_json,
        model,
        token_usage_json,
        ..
    } = checkpoint;
    let mut unproven = active_messages.into_iter();
    let mut base = CheckpointBase {
        messages: Vec::new(),
        lineage: Vec::new(),
        summary: None,
        unproven: Vec::new(),
    };

    if let Some(summary_json) = summary_json {
        let summary = serde_json::from_value::<ContinuitySummary>(summary_json)
            .map_err(|error| SessionResumeError::Decode(error.to_string()))?;
        // The store enforces all-or-none summary provenance at the write
        // boundary; absence here means the row lost integrity.
        let model = model.ok_or_else(|| {
            SessionResumeError::Decode("summary checkpoint lacks its model".to_string())
        })?;
        let usage = token_usage_json
            .ok_or_else(|| {
                SessionResumeError::Decode("summary checkpoint lacks token usage".to_string())
            })
            .and_then(|value| {
                serde_json::from_value::<Usage>(value)
                    .map_err(|error| SessionResumeError::Decode(error.to_string()))
            })?;
        let projected = project_summary_message(&summary)
            .map_err(|error| SessionResumeError::Decode(error.to_string()))?;
        if unproven.next().as_ref() != Some(&projected) {
            return Err(SessionResumeError::SummaryProjectionMismatch);
        }
        let refs = summary.artifact_refs.iter().cloned().collect();
        let coverage = MessageCoverage::closed(summary.source_seq_start, summary.source_seq_end);
        base.messages.push(projected);
        base.lineage
            .push(MessageLineage::derived(coverage, false, refs));
        base.summary = Some(ActiveSummary::new(summary, model, usage));
    }

    base.unproven = unproven.collect();
    Ok(base)
}

/// Finds the journal fact that explains one checkpoint message.
///
/// Active messages preserve journal order, so a single forward cursor over the
/// facts at or below `covers` suffices. A byte-equal fact proves the message
/// is verbatim journal content; a fact carrying the same tool-result ids
/// proves a deterministic rewrite of that exact exchange, and recomputing the
/// original result hashes recovers which committed artifacts the rewrite may
/// reference. Facts skipped along the way belong to messages that compaction
/// absorbed.
fn match_journal_fact(
    facts: &[(Seq, Message)],
    cursor: &mut usize,
    covers: Seq,
    message: &Message,
    committed_artifacts: &BTreeSet<String>,
) -> Option<MessageLineage> {
    let message_result_ids = tool_result_ids(message);
    while let Some((seq, fact)) = facts.get(*cursor) {
        if *seq > covers {
            return None;
        }
        *cursor += 1;
        if fact == message {
            return Some(MessageLineage::appended(Some(*seq), false));
        }
        if !message_result_ids.is_empty() && tool_result_ids(fact) == message_result_ids {
            let refs = tool_results(fact)
                .filter_map(|result| tool_result_hash(result).ok())
                .map(|hash| format!("tool-result-{hash}"))
                .filter(|artifact_id| committed_artifacts.contains(artifact_id))
                .collect();
            return Some(MessageLineage::derived(
                MessageCoverage::exact(*seq),
                false,
                refs,
            ));
        }
    }
    None
}

fn tool_results(message: &Message) -> impl Iterator<Item = &ToolResult> {
    let content = match message {
        Message::User { content } => Some(content.iter()),
        Message::System { .. } | Message::Assistant { .. } => None,
    };
    content
        .into_iter()
        .flatten()
        .filter_map(|block| match block {
            UserContent::ToolResult(result) => Some(result),
            UserContent::Text(_) => None,
        })
}

fn tool_result_ids(message: &Message) -> BTreeSet<&str> {
    tool_results(message)
        .map(|result| result.id.as_str())
        .collect()
}

/// Failure to rebuild a session from durable state.
///
/// Every variant leaves the store untouched; resume is read-only.
#[derive(Debug, thiserror::Error)]
pub enum SessionResumeError {
    /// The store could not be read.
    #[error(transparent)]
    Store(#[from] SessionStoreError),
    /// A durable payload failed structural decoding.
    #[error("failed to decode durable session state: {0}")]
    Decode(String),
    /// The checkpoint's summary provenance does not reproduce its head message.
    #[error("checkpoint summary provenance does not reproduce its active context")]
    SummaryProjectionMismatch,
    /// An active message is not explained by any journal fact it could cover.
    #[error("checkpoint message {index} has no matching journal fact")]
    UnmatchedCheckpointMessage {
        /// Position among the checkpoint messages that follow the summary head.
        index: usize,
    },
}

#[cfg(test)]
mod tests {
    use kuncode_core::{
        completion::{ToolResultContent, Usage},
        non_empty_vec::NonEmptyVec,
    };

    use super::*;
    use crate::{
        compaction::summary::{CONTINUITY_SUMMARY_VERSION, WorkspaceSummary},
        session_store::{
            NewCheckpoint, NewJournalEntry, NewSession, NewToolArtifact, turso::TursoSessionStore,
        },
        test_support::TestDir,
    };

    async fn store_with_session(root: &TestDir) -> (TursoSessionStore, SessionId) {
        let store = TursoSessionStore::open(root.path().join("sessions.db"))
            .await
            .expect("store should open");
        let session = store
            .create_session(NewSession::new(root.path().to_path_buf()))
            .await
            .expect("session should be created");
        (store, session)
    }

    async fn append_message(
        store: &TursoSessionStore,
        session: &SessionId,
        message: &Message,
    ) -> Seq {
        store
            .append(
                session,
                NewJournalEntry::message(message).expect("message should encode"),
            )
            .await
            .expect("append should commit")
    }

    fn summary_fixture(start: i64, end: i64, artifact_refs: Vec<String>) -> ContinuitySummary {
        ContinuitySummary {
            version: CONTINUITY_SUMMARY_VERSION,
            source_seq_start: Seq::new(start),
            source_seq_end: Seq::new(end),
            current_goal: "resume the session".to_string(),
            constraints: vec![],
            decisions: vec![],
            completed_work: vec![],
            workspace: WorkspaceSummary {
                working_directory: "/workspace".to_string(),
                files: vec![],
                symbols: vec![],
            },
            commands_and_tests: vec![],
            unresolved_errors: vec![],
            todos: vec![],
            next_actions: vec![],
            artifact_refs,
        }
    }

    #[tokio::test]
    async fn resume_replays_a_journal_without_checkpoints() {
        let root = TestDir::new();
        let (store, id) = store_with_session(&root).await;
        let user = Message::user("hello");
        let assistant = Message::assistant("hi");
        append_message(&store, &id, &user).await;
        append_message(&store, &id, &assistant).await;

        let mut session =
            AgentSession::resume_durable_session(&store, id.clone(), PermissionMode::Default)
                .await
                .expect("resume should rebuild the session");

        assert_eq!(session.messages(), &[user, assistant]);
        assert_eq!(session.session_id(), Some(&id));
        assert_eq!(session.durable_seq(), Some(Seq::new(2)));
        assert_eq!(
            session.message_lineage()[0].verbatim_journal_seq(),
            Some(Seq::new(1))
        );
        // The rebuilt session is attached: in-memory appends without a durable
        // receipt must stay rejected exactly as in the original process.
        assert!(session.push_user("no receipt").is_err());
    }

    #[tokio::test]
    async fn resume_restores_summary_head_and_live_tail() {
        let root = TestDir::new();
        let (store, id) = store_with_session(&root).await;
        for text in ["one", "two", "three"] {
            append_message(&store, &id, &Message::user(text)).await;
        }
        let retained = Message::user("four");
        append_message(&store, &id, &retained).await;
        let summary = summary_fixture(1, 3, vec!["tool-result-sha256-abc".to_string()]);
        let projected = project_summary_message(&summary).expect("summary should project");
        store
            .write_checkpoint(NewCheckpoint {
                session_id: id.clone(),
                covers_through_seq: Seq::new(4),
                source_seq_start: Some(Seq::new(1)),
                source_seq_end: Some(Seq::new(3)),
                active_messages: vec![projected.clone(), retained.clone()],
                summary_json: Some(serde_json::to_value(&summary).expect("summary should encode")),
                model: Some("summary-model".to_string()),
                token_usage_json: Some(
                    serde_json::to_value(Usage::default()).expect("usage should encode"),
                ),
            })
            .await
            .expect("checkpoint should commit");
        let tail = Message::assistant("after the checkpoint");
        let tail_seq = append_message(&store, &id, &tail).await;

        let session = AgentSession::resume_durable_session(&store, id, PermissionMode::Default)
            .await
            .expect("resume should rebuild the session");

        assert_eq!(session.messages(), &[projected, retained, tail]);
        assert_eq!(session.durable_seq(), Some(tail_seq));
        let head = &session.message_lineage()[0];
        let coverage = head.coverage().expect("summary head carries coverage");
        assert_eq!(
            (coverage.start(), coverage.end()),
            (Seq::new(1), Seq::new(3))
        );
        assert!(head.artifact_refs().contains("tool-result-sha256-abc"));
        let restored = session
            .active_summary_record()
            .expect("summary provenance should be restored");
        assert_eq!(restored.model(), "summary-model");
        assert_eq!(
            session.message_lineage()[1].verbatim_journal_seq(),
            Some(Seq::new(4))
        );
        assert_eq!(
            session.message_lineage()[2].verbatim_journal_seq(),
            Some(tail_seq)
        );
    }

    #[tokio::test]
    async fn resume_recovers_artifact_refs_for_rewritten_results() {
        let root = TestDir::new();
        let (store, id) = store_with_session(&root).await;
        let prompt = Message::user("run the tool");
        append_message(&store, &id, &prompt).await;
        let result = ToolResult {
            id: "call-1".to_string(),
            call_id: None,
            content: NonEmptyVec::new(ToolResultContent::text("the complete tool output")),
        };
        let full = Message::User {
            content: NonEmptyVec::new(UserContent::ToolResult(result.clone())),
        };
        let result_seq = append_message(&store, &id, &full).await;
        let hash = tool_result_hash(&result).expect("result should hash");
        let artifact_id = format!("tool-result-{hash}");
        store
            .put_tool_artifact(
                &id,
                result_seq,
                NewToolArtifact::inline(
                    &hash,
                    "preview",
                    serde_json::to_string(&result).expect("result should encode"),
                )
                .expect("artifact should validate"),
            )
            .await
            .expect("artifact should commit");
        let slimmed = Message::tool_result("call-1", "preview only");
        store
            .write_checkpoint(NewCheckpoint {
                session_id: id.clone(),
                covers_through_seq: Seq::new(3),
                source_seq_start: None,
                source_seq_end: None,
                active_messages: vec![prompt.clone(), slimmed.clone()],
                summary_json: None,
                model: None,
                token_usage_json: None,
            })
            .await
            .expect("checkpoint should commit");

        let session = AgentSession::resume_durable_session(&store, id, PermissionMode::Default)
            .await
            .expect("resume should rebuild the session");

        assert_eq!(session.messages(), &[prompt, slimmed]);
        let rewritten = &session.message_lineage()[1];
        let coverage = rewritten.coverage().expect("rewrite keeps exact coverage");
        assert_eq!((coverage.start(), coverage.end()), (result_seq, result_seq));
        assert!(
            rewritten.verbatim_journal_seq().is_none(),
            "a rewritten result must not claim verbatim journal authority"
        );
        assert!(rewritten.artifact_refs().contains(&artifact_id));
    }

    #[tokio::test]
    async fn resume_fails_closed_on_an_unexplained_checkpoint_message() {
        let root = TestDir::new();
        let (store, id) = store_with_session(&root).await;
        append_message(&store, &id, &Message::user("journaled")).await;
        store
            .write_checkpoint(NewCheckpoint {
                session_id: id.clone(),
                covers_through_seq: Seq::new(1),
                source_seq_start: None,
                source_seq_end: None,
                active_messages: vec![Message::user("never journaled")],
                summary_json: None,
                model: None,
                token_usage_json: None,
            })
            .await
            .expect("checkpoint should commit");

        let error = AgentSession::resume_durable_session(&store, id, PermissionMode::Default)
            .await
            .expect_err("an unexplained message must fail resume");

        assert!(matches!(
            error,
            SessionResumeError::UnmatchedCheckpointMessage { index: 0 }
        ));
    }
}
