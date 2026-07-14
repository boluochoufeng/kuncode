use kuncode_core::completion::Message;

use super::{CandidateLoad, SelectionLimits, SelectionOutcome, select_prefix_tail};
use crate::compaction::protocol::{
    HumanMessageIndex, group_messages, select_protected_recent_tail,
};

mod boundaries;

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

pub(super) fn history() -> Vec<Message> {
    vec![
        Message::user("fix it"),
        Message::assistant("working"),
        Message::assistant("recent"),
    ]
}
