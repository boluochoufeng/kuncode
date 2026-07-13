//! Cancellation race shared by model, hook, and tool awaits.

use std::future::Future;

use tokio_util::sync::CancellationToken;

/// Races `fut` against `cancel`, returning `None` if the token fires first and
/// `Some(output)` otherwise.
///
/// The single home for the loop's cancellation race: every interruptible await
/// point (model request, tool execution, each hook) goes through here, so the
/// contract lives in one place rather than re-spelled at each site:
///
/// - **`biased`** — the cancel branch is polled first, so an already-cancelled
///   token wins deterministically and the future is never started.
/// - **drop cancels** — losing the race drops `fut`, cancelling any in-flight
///   work it owns (the provider's HTTP call, a child process, a hook's shell-out).
/// - **`None` means cancelled** — the caller owns what to do then (unwind, pair
///   remaining tool_calls, emit a terminal error); this helper deliberately does
///   no cleanup, since each site's is different.
pub(super) async fn cancellable<T>(
    cancel: &CancellationToken,
    fut: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        output = fut => Some(output),
    }
}
