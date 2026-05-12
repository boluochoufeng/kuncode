//! The Phase 2 capability gate.
//!
//! Plan §11.3:
//!
//! ```text
//! allowed = descriptor.default_capabilities intersects granted_capabilities
//! ```
//!
//! Risk flags are recorded but not enforced here — `Ask` / `Deny` policy lives
//! in Phase 5.

use kuncode_core::ToolCapability;

/// Returns `true` when `granted` shares at least one capability with `required`.
/// An empty `required` is treated as always-deny: `ToolDescriptor::validate`
/// already rejects empty `default_capabilities` at registration time, so this
/// path should be unreachable in practice.
pub fn is_allowed(required: &[ToolCapability], granted: &[ToolCapability]) -> bool {
    granted.iter().any(|cap| required.contains(cap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ToolCapability::*;

    #[test]
    fn allows_when_capabilities_overlap() {
        assert!(is_allowed(&[Explore], &[Explore]));
        assert!(is_allowed(&[Explore, Verify], &[Edit, Explore]));
        assert!(is_allowed(&[Lead], &[Lead, Edit, Verify, Explore]));
    }

    #[test]
    fn denies_when_no_overlap() {
        assert!(!is_allowed(&[Edit], &[Explore]));
        assert!(!is_allowed(&[Lead], &[Explore, Verify]));
    }

    #[test]
    fn denies_when_granted_is_empty() {
        assert!(!is_allowed(&[Explore], &[]));
    }

    #[test]
    fn denies_when_required_is_empty() {
        assert!(!is_allowed(&[], &[Explore, Edit, Verify, Lead]));
    }

    #[test]
    fn duplicates_do_not_change_outcome() {
        assert!(is_allowed(&[Explore, Explore], &[Explore]));
        assert!(!is_allowed(&[Edit, Edit], &[Explore, Explore]));
    }
}
