//! Context compaction domain types and deterministic request accounting.

pub mod artifact;
pub mod budget;
mod orchestrator;
pub mod protocol;
pub mod selection;
pub mod slimming;
pub mod summary;

pub(crate) use orchestrator::{
    CompactionDependencies, CompactionError, CompactionOutcome, CompactionRequestProjector,
    GroupTokenEstimator, RequestProjectionError, compact_context,
};
