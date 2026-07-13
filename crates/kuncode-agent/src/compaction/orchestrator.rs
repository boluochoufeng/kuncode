//! Crash-safe orchestration of deterministic and semantic compaction passes.

mod candidate;
mod pipeline;
mod types;

pub(crate) use pipeline::compact_context;
#[cfg(test)]
pub(crate) use types::CompactionPass;
pub(crate) use types::{
    CompactionDependencies, CompactionError, CompactionOutcome, CompactionRequestProjector,
    GroupTokenEstimator, RequestProjectionError,
};

#[cfg(test)]
mod tests;
