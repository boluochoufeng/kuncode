//! Durable-first spilling of old tool results into compact markers.

mod audit;
mod boundary;
mod hash;
mod marker;
mod preview;
mod spill;
mod types;

pub use boundary::{ArtifactSpillError, ArtifactSpillInput};
pub use spill::spill_artifacts;
pub use types::{
    ArtifactSpillFailure, ArtifactSpillOutcome, ArtifactSpillResult, ArtifactStore,
    ArtifactTokenCounter, ArtifactTokenCounterError,
};

#[cfg(test)]
mod tests;
