//! Trusted in-memory provenance carried alongside provider-visible messages.

use std::collections::BTreeSet;

use kuncode_core::completion::{Message, Usage};

use crate::compaction::summary::ContinuitySummary;
use crate::session_store::Seq;
use crate::tool::ToolResultRetention;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ActiveSummary {
    summary: ContinuitySummary,
    model: String,
    usage: Usage,
}

impl ActiveSummary {
    pub(crate) fn new(summary: ContinuitySummary, model: String, usage: Usage) -> Self {
        Self {
            summary,
            model,
            usage,
        }
    }

    pub(crate) const fn summary(&self) -> &ContinuitySummary {
        &self.summary
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) const fn usage(&self) -> Usage {
        self.usage
    }
}

/// Closed durable journal range represented by one active message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MessageCoverage {
    start: Seq,
    end: Seq,
}

impl MessageCoverage {
    /// Binds an ordinary durable append to its single journal fact.
    pub(crate) const fn exact(seq: Seq) -> Self {
        Self {
            start: seq,
            end: seq,
        }
    }

    /// Creates an ordered non-empty range already validated by the compactor.
    pub(crate) const fn closed(start: Seq, end: Seq) -> Self {
        Self { start, end }
    }

    /// Returns the first represented durable fact.
    pub(crate) const fn start(self) -> Seq {
        self.start
    }

    /// Returns the last represented durable fact.
    pub(crate) const fn end(self) -> Seq {
        self.end
    }
}

/// Harness-owned provenance for one position in the active message vector.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MessageLineage {
    coverage: Option<MessageCoverage>,
    human_authored: bool,
    artifact_refs: BTreeSet<String>,
    journal_verbatim: bool,
    tool_result_retention: ToolResultRetention,
}

impl MessageLineage {
    /// Records facts known at the live append boundary.
    pub(crate) fn appended(journal_seq: Option<Seq>, human_authored: bool) -> Self {
        Self {
            coverage: journal_seq.map(MessageCoverage::exact),
            human_authored,
            artifact_refs: BTreeSet::new(),
            journal_verbatim: journal_seq.is_some(),
            tool_result_retention: ToolResultRetention::Verbatim,
        }
    }

    /// Carries audited source coverage through a derived active-context message.
    pub(crate) fn derived(
        coverage: MessageCoverage,
        human_authored: bool,
        artifact_refs: BTreeSet<String>,
    ) -> Self {
        Self {
            coverage: Some(coverage),
            human_authored,
            artifact_refs,
            journal_verbatim: false,
            tool_result_retention: ToolResultRetention::Verbatim,
        }
    }

    pub(crate) const fn with_tool_result_retention(
        mut self,
        retention: ToolResultRetention,
    ) -> Self {
        self.tool_result_retention = retention;
        self
    }

    /// Returns the durable range represented by this message, when proven.
    pub(crate) const fn coverage(&self) -> Option<MessageCoverage> {
        self.coverage
    }

    /// Distinguishes direct turn input from user-role harness injections.
    pub(crate) const fn human_authored(&self) -> bool {
        self.human_authored
    }

    /// Returns artifact identifiers already authorized for this message.
    pub(crate) const fn artifact_refs(&self) -> &BTreeSet<String> {
        &self.artifact_refs
    }

    /// Returns a sequence only for an unchanged message backed by one journal fact.
    pub(crate) fn verbatim_journal_seq(&self) -> Option<Seq> {
        match (self.journal_verbatim, self.coverage) {
            (true, Some(coverage)) if coverage.start == coverage.end => Some(coverage.start),
            (true, Some(_)) | (true, None) | (false, _) => None,
        }
    }

    pub(crate) const fn tool_result_retention(&self) -> ToolResultRetention {
        self.tool_result_retention
    }
}

/// Complete in-memory state authorized for one receipt-bound installation.
pub(crate) struct PreparedActiveContext {
    pub(super) messages: Vec<Message>,
    pub(super) lineage: Vec<MessageLineage>,
    pub(super) summary: Option<ActiveSummary>,
}

impl PreparedActiveContext {
    pub(crate) fn new(
        messages: Vec<Message>,
        lineage: Vec<MessageLineage>,
        summary: Option<ActiveSummary>,
    ) -> Option<Self> {
        (!messages.is_empty() && messages.len() == lineage.len()).then_some(Self {
            messages,
            lineage,
            summary,
        })
    }
}

pub(super) fn untrusted_lineage(len: usize) -> Vec<MessageLineage> {
    vec![MessageLineage::default(); len]
}
