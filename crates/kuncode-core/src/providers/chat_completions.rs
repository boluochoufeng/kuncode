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
