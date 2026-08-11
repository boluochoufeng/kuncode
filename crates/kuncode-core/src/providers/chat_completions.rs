//! Shared transport primitives for Chat Completions protocol providers.

use std::time::Duration;

pub(crate) mod streaming;

/// Bound on dialing a Chat Completions endpoint.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum idle gap between response-body chunks.
///
/// This catches stalled streams without imposing a total generation deadline.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(360);
/// Total deadline for a non-streaming Chat Completions request.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(360);

/// Warns when the provider reports having served a different model than the
/// request named.
///
/// The response's `model` field is the provider's own account of what ran, so
/// a mismatch is a tripwire for silent server-side aliasing or routing that no
/// client-side check can catch. Quiet when the provider omits the field —
/// absence is not evidence of a mismatch.
pub(crate) fn check_served_model(requested: &str, served: Option<&str>) {
    if let Some(served) = served
        && served != requested
    {
        tracing::warn!(
            target: "kuncode::provider",
            requested,
            served,
            "provider served a different model than requested",
        );
    }
}
