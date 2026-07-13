//! Protocol-safe grouping and protection boundaries for context compaction.

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
