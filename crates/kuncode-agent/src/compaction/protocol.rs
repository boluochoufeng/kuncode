//! Protocol-safe grouping and non-empty suffix protection for context compaction.
//!
//! Lossy passes operate on canonical groups so an assistant tool request and
//! all of its results are retained, moved, or summarized as one closed unit.

mod grouping;
mod protection;

pub(crate) use grouping::flatten_groups;
pub use grouping::{ProtocolError, ProtocolGroup, group_messages};
pub(crate) use protection::select_protected_recent_tail_from_estimates;
pub use protection::{
    HumanMessageIndex, HumanRequestAnchor, ProtectedRecentTail, current_human_request_anchor,
    select_protected_recent_tail,
};

#[cfg(test)]
mod tests;
