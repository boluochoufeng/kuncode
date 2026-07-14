//! Target-aware selection of a complete summary prefix and verbatim tail.
//!
//! The target is an optimization stop, not a validity boundary: selection only
//! requests semantic work when a complete unprotected prefix remains and the
//! deterministic candidate has not reached that target.

use kuncode_core::completion::Message;
use thiserror::Error;

use crate::compaction::protocol::{
    HumanMessageIndex, HumanRequestAnchor, ProtectedRecentTail, ProtocolError, ProtocolGroup,
    current_human_request_anchor, flatten_groups, group_messages,
};

/// Exact token limits derived from one validated runtime window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionLimits {
    target_tokens: u64,
    soft_tokens: u64,
}

impl SelectionLimits {
    /// Creates ordered target and soft boundaries.
    ///
    /// # Errors
    /// Returns [`SelectionError::InvalidLimits`] unless `0 < target < soft`.
    pub fn new(target_tokens: u64, soft_tokens: u64) -> Result<Self, SelectionError> {
        if target_tokens == 0 || target_tokens >= soft_tokens {
            return Err(SelectionError::InvalidLimits);
        }
        Ok(Self {
            target_tokens,
            soft_tokens,
        })
    }
}

/// Validity class for one provider-visible candidate count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateLoad {
    /// Optimization target reached; later deterministic passes may stop.
    TargetReached,
    /// Above target but still below the strict soft validity boundary.
    BelowSoft,
    /// At or above soft and invalid without further compaction.
    RequiresCompaction,
}

impl CandidateLoad {
    /// Classifies exact token counts without treating target as a hard gate.
    pub const fn classify(tokens: u64, limits: SelectionLimits) -> Self {
        if tokens <= limits.target_tokens {
            Self::TargetReached
        } else if tokens < limits.soft_tokens {
            Self::BelowSoft
        } else {
            Self::RequiresCompaction
        }
    }
}

/// Protocol-safe split awaiting durable source binding by the orchestrator.
///
/// Both sides contain whole canonical groups. The prefix is the complete
/// unprotected history; the protected suffix is retained byte-for-byte.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionSelection {
    summarize: Vec<ProtocolGroup>,
    retain_verbatim: Vec<ProtocolGroup>,
    current_request_anchor: Option<HumanRequestAnchor>,
}

impl CompactionSelection {
    /// Returns the contiguous complete prefix delegated to summarization.
    pub fn summarize(&self) -> &[ProtocolGroup] {
        &self.summarize
    }

    /// Returns the contiguous protected suffix retained exactly.
    pub fn retain_verbatim(&self) -> &[ProtocolGroup] {
        &self.retain_verbatim
    }

    /// Returns the exact current human request when it lies in the prefix.
    pub fn current_request_anchor(&self) -> Option<&Message> {
        self.current_request_anchor
            .as_ref()
            .map(|anchor| &anchor.message)
    }

    pub(crate) fn current_request_anchor_index(&self) -> Option<usize> {
        self.current_request_anchor
            .as_ref()
            .map(|anchor| anchor.source_message_index)
    }
}

/// Result of selection after deterministic passes have been remeasured.
#[derive(Clone, Debug, PartialEq)]
pub enum SelectionOutcome {
    /// Existing deterministic candidate can proceed directly to validation.
    DeterministicCandidate {
        /// Final classification of the deterministic candidate.
        load: CandidateLoad,
    },
    /// A non-empty safe prefix requires semantic summarization.
    Summarize(CompactionSelection),
    /// No group boundary can produce a candidate below soft.
    Uncompressible {
        /// Invalid load that cannot be reduced at a safe group boundary.
        load: CandidateLoad,
    },
}

/// Invalid caller-provided selection inputs.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SelectionError {
    /// Target and soft token boundaries are invalid.
    #[error("selection requires ordered non-zero target and soft token limits")]
    InvalidLimits,
    /// Protected tail is not a non-empty suffix of supplied groups.
    #[error("protected recent tail is not a non-empty suffix of selection groups")]
    InvalidProtectedTail,
    /// Candidate groups are not the canonical closure of their messages.
    #[error("selection requires canonical closed protocol groups")]
    InvalidProtocolGroups,
    /// Candidate and authoritative histories do not preserve message positions.
    #[error("selection candidate message count differs from authoritative active context")]
    ActiveMessageCountMismatch,
    /// Candidate projection changed a message inside the protected suffix.
    #[error("selection candidate changed the protected recent tail")]
    ProtectedTailChanged,
    /// Human provenance is invalid for the authoritative active context.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

/// Selects the complete unprotected prefix without splitting protocol groups.
///
/// A deterministic candidate that reaches the target is accepted immediately.
/// If no safe prefix remains, a candidate below soft is still valid because the
/// target is not a hard boundary; otherwise the input is uncompressible.
///
/// # Errors
/// Returns [`SelectionError`] when protocol groups, the protected suffix,
/// authoritative messages, or human provenance disagree.
pub fn select_prefix_tail(
    groups: &[ProtocolGroup],
    authoritative_messages: &[Message],
    protected: &ProtectedRecentTail,
    human_messages: &[HumanMessageIndex],
    limits: SelectionLimits,
    candidate_tokens: u64,
) -> Result<SelectionOutcome, SelectionError> {
    if groups.is_empty()
        || protected.group_range.end != groups.len()
        || protected.group_range.start >= protected.group_range.end
        || groups
            .iter()
            .rposition(|group| matches!(group, ProtocolGroup::ToolExchange { .. }))
            .is_some_and(|mandatory| protected.group_range.start > mandatory)
    {
        return Err(SelectionError::InvalidProtectedTail);
    }
    let flattened = flatten(groups);
    if group_messages(&flattened).map_or(true, |regrouped| regrouped != groups) {
        return Err(SelectionError::InvalidProtocolGroups);
    }
    if flattened.len() != authoritative_messages.len() {
        return Err(SelectionError::ActiveMessageCountMismatch);
    }
    let prefix_message_end = flatten(&groups[..protected.group_range.start]).len();
    if flattened[prefix_message_end..] != authoritative_messages[prefix_message_end..] {
        return Err(SelectionError::ProtectedTailChanged);
    }
    let load = CandidateLoad::classify(candidate_tokens, limits);
    if load == CandidateLoad::TargetReached {
        return Ok(SelectionOutcome::DeterministicCandidate { load });
    }
    if protected.group_range.start == 0 {
        return Ok(if load == CandidateLoad::BelowSoft {
            SelectionOutcome::DeterministicCandidate { load }
        } else {
            SelectionOutcome::Uncompressible { load }
        });
    }
    let anchor =
        current_human_request_anchor(authoritative_messages, human_messages, prefix_message_end)?;
    Ok(SelectionOutcome::Summarize(CompactionSelection {
        summarize: groups[..protected.group_range.start].to_vec(),
        retain_verbatim: groups[protected.group_range.clone()].to_vec(),
        current_request_anchor: anchor,
    }))
}

fn flatten(groups: &[ProtocolGroup]) -> Vec<Message> {
    flatten_groups(groups)
}

#[cfg(test)]
mod tests {
    use kuncode_core::{
        completion::{AssistantContent, Message, ToolResult, ToolResultContent, UserContent},
        non_empty_vec::NonEmptyVec,
    };

    use super::{
        CandidateLoad, SelectionError, SelectionLimits, SelectionOutcome, select_prefix_tail,
    };
    use crate::compaction::protocol::{
        HumanMessageIndex, ProtectedRecentTail, ProtocolGroup, group_messages,
        select_protected_recent_tail,
    };

    #[test]
    fn target_stops_deterministic_passes_but_target_soft_candidate_remains_valid() {
        // Given: target and soft are separate optimization and validity boundaries.
        let limits = SelectionLimits::new(50, 75).expect("limits should be ordered");

        // When: artifact, slimming, and between-boundary counts are classified.
        let after_artifact = CandidateLoad::classify(50, limits);
        let after_slimming = CandidateLoad::classify(49, limits);
        let between = CandidateLoad::classify(60, limits);

        // Then: deterministic passes stop at target while target-soft stays valid.
        assert_eq!(after_artifact, CandidateLoad::TargetReached);
        assert_eq!(after_slimming, CandidateLoad::TargetReached);
        assert_eq!(between, CandidateLoad::BelowSoft);
        assert_eq!(
            CandidateLoad::classify(75, limits),
            CandidateLoad::RequiresCompaction
        );
    }

    #[test]
    fn selects_complete_old_prefix_and_exact_protected_tail_for_summary() {
        // Given: an old human anchor and a mandatory recent closed exchange.
        let messages = history();
        let groups = group_messages(&messages).expect("ordinary history should group");
        let protected = select_protected_recent_tail(&groups, 0, |_| 1)
            .expect("history should have a protected tail");
        // When: the remeasured candidate remains above soft.
        let outcome = select_prefix_tail(
            &groups,
            &messages,
            &protected,
            &[HumanMessageIndex(0)],
            SelectionLimits::new(50, 75).expect("limits should be ordered"),
            75,
        )
        .expect("closed history should be selectable");

        // Then: summary and retention are contiguous and the human anchor is exact.
        let SelectionOutcome::Summarize(selection) = outcome else {
            panic!("old prefix should require a semantic summary");
        };
        assert_eq!(
            selection.summarize(),
            &groups[..protected.group_range.start]
        );
        assert_eq!(
            selection.retain_verbatim(),
            &groups[protected.group_range.clone()]
        );
        assert_eq!(
            selection.current_request_anchor(),
            Some(&Message::user("fix it"))
        );
    }

    #[test]
    fn safe_prefix_above_target_requires_summary_even_when_below_soft() {
        // Given: a valid deterministic candidate between target and soft with old prefix.
        let messages = history();
        let groups = group_messages(&messages).expect("ordinary history should group");
        let protected = select_protected_recent_tail(&groups, 0, |_| 1)
            .expect("history should have a protected tail");

        // When: prefix selection runs at target < load < soft.
        let outcome = select_prefix_tail(
            &groups,
            &messages,
            &protected,
            &[HumanMessageIndex(0)],
            SelectionLimits::new(50, 75).expect("limits should be ordered"),
            60,
        )
        .expect("closed history should be selectable");

        // Then: the non-empty safe prefix still proceeds to semantic summary.
        assert!(matches!(outcome, SelectionOutcome::Summarize(_)));
    }

    #[test]
    fn only_protected_group_is_never_split_to_satisfy_soft() {
        // Given: history consisting only of one mandatory group.
        let groups =
            group_messages(&[Message::user("current")]).expect("ordinary history should group");
        let messages = vec![Message::user("current")];
        let protected = select_protected_recent_tail(&groups, 0, |_| 100)
            .expect("history should have a protected tail");
        let limits = SelectionLimits::new(50, 75).expect("limits should be ordered");

        // When: the mandatory group alone is over soft.
        let outcome = select_prefix_tail(
            &groups,
            &messages,
            &protected,
            &[HumanMessageIndex(0)],
            limits,
            80,
        )
        .expect("protected-only history is structurally valid");

        // Then: selection reports an uncompressible candidate instead of splitting it.
        assert_eq!(
            outcome,
            SelectionOutcome::Uncompressible {
                load: CandidateLoad::RequiresCompaction,
            }
        );
    }

    #[test]
    fn no_safe_prefix_below_soft_skips_summary() {
        // Given: one mandatory group already below soft after deterministic passes.
        let groups =
            group_messages(&[Message::user("current")]).expect("ordinary history should group");
        let messages = vec![Message::user("current")];
        let protected = select_protected_recent_tail(&groups, 0, |_| 60)
            .expect("history should have a protected tail");

        // When: selection sees no non-empty old prefix.
        let outcome = select_prefix_tail(
            &groups,
            &messages,
            &protected,
            &[HumanMessageIndex(0)],
            SelectionLimits::new(50, 75).expect("limits should be ordered"),
            60,
        )
        .expect("protected-only history is structurally valid");

        // Then: the valid deterministic candidate proceeds without an LLM summary.
        assert_eq!(
            outcome,
            SelectionOutcome::DeterministicCandidate {
                load: CandidateLoad::BelowSoft,
            }
        );
    }

    fn history() -> Vec<Message> {
        vec![
            Message::user("fix it"),
            Message::assistant("working"),
            Message::assistant("recent"),
        ]
    }

    #[test]
    fn exact_target_with_safe_prefix_skips_summary() {
        // Given: closed history with a non-empty old prefix.
        let messages = history();
        let groups = group_messages(&messages).expect("ordinary history should group");
        let protected = select_protected_recent_tail(&groups, 0, |_| 1)
            .expect("history should have a protected tail");

        // When: deterministic projection reaches the exact optimization target.
        let outcome = select_prefix_tail(
            &groups,
            &messages,
            &protected,
            &[HumanMessageIndex(0)],
            SelectionLimits::new(50, 75).expect("limits should be ordered"),
            50,
        )
        .expect("closed history should be selectable");

        // Then: later lossy passes stop before semantic summary.
        assert_eq!(
            outcome,
            SelectionOutcome::DeterministicCandidate {
                load: CandidateLoad::TargetReached,
            }
        );
    }

    #[test]
    fn human_anchor_comes_from_authoritative_messages_not_projected_prefix() {
        // Given: a deterministic projection changed old prefix content only.
        let messages = history();
        let mut groups = group_messages(&messages).expect("ordinary history should group");
        groups[0] = ProtocolGroup::Message(Message::user("projected surrogate"));
        let protected = select_protected_recent_tail(&groups, 0, |_| 1)
            .expect("history should have a protected tail");

        // When: selection builds the summary input and current-request anchor.
        let outcome = select_prefix_tail(
            &groups,
            &messages,
            &protected,
            &[HumanMessageIndex(0)],
            SelectionLimits::new(50, 75).expect("limits should be ordered"),
            60,
        )
        .expect("prefix-only projection should be selectable");

        // Then: candidate prefix is summarized but the anchor remains exact source text.
        let SelectionOutcome::Summarize(selection) = outcome else {
            panic!("safe old prefix should require summary");
        };
        assert_eq!(selection.summarize(), &groups[..2]);
        assert_eq!(
            selection.current_request_anchor(),
            Some(&Message::user("fix it"))
        );
    }

    #[test]
    fn changed_protected_tail_is_rejected_before_summary() {
        // Given: candidate groups changed a message inside the protected suffix.
        let messages = history();
        let mut groups = group_messages(&messages).expect("ordinary history should group");
        groups[2] = ProtocolGroup::Message(Message::assistant("rewritten recent"));
        let protected = select_protected_recent_tail(&groups, 0, |_| 1)
            .expect("history should have a protected tail");

        // When: selection validates candidate retention against authoritative history.
        let result = select_prefix_tail(
            &groups,
            &messages,
            &protected,
            &[HumanMessageIndex(0)],
            SelectionLimits::new(50, 75).expect("limits should be ordered"),
            75,
        );

        // Then: protected-tail loss fails closed without producing a selection.
        assert_eq!(result, Err(SelectionError::ProtectedTailChanged));
    }

    #[test]
    fn forged_tail_cannot_omit_the_latest_tool_exchange() {
        let messages = vec![
            Message::Assistant {
                id: None,
                content: NonEmptyVec::new(AssistantContent::tool_call(
                    "call",
                    "read_file",
                    serde_json::json!({"path": "src/lib.rs"}),
                )),
            },
            Message::User {
                content: NonEmptyVec::new(UserContent::ToolResult(ToolResult {
                    id: "call".to_string(),
                    call_id: None,
                    content: NonEmptyVec::new(ToolResultContent::text("result")),
                })),
            },
            Message::assistant("after tools"),
        ];
        let groups = group_messages(&messages).expect("fixture should be canonical");
        let protected = ProtectedRecentTail {
            group_range: 1..2,
            estimated_tokens: 1,
            budget_tokens: 1,
        };

        let result = select_prefix_tail(
            &groups,
            &messages,
            &protected,
            &[],
            SelectionLimits::new(50, 75).expect("limits should be ordered"),
            60,
        );

        assert_eq!(result, Err(SelectionError::InvalidProtectedTail));
    }
}
