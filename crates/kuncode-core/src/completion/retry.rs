//! A [`CompletionModel`] decorator that retries transient provider failures.
//!
//! Wrap the real model once at construction with [`RetryModel`]; every caller —
//! the agent loop today, and future model calls like context-compaction
//! summaries or subagents — then inherits exponential-backoff retries without
//! knowing they exist. Retry policy lives in [`RetryPolicy`].

use std::time::Duration;

use crate::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, CompletionStream,
};

/// Exponential-backoff schedule for [`RetryModel`].
///
/// Delays grow geometrically from [`base_delay`](Self::base_delay) by
/// [`multiplier`](Self::multiplier) and saturate at [`max_delay`](Self::max_delay).
/// No jitter: a single-client CLI has no thundering herd to spread out, and
/// skipping it keeps `core` free of a randomness dependency.
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Retries attempted *after* the initial call, so total attempts =
    /// `max_retries + 1`. Zero disables retrying (a single attempt).
    pub max_retries: u32,
    /// Delay before the first retry.
    pub base_delay: Duration,
    /// Geometric growth factor applied once per elapsed retry.
    pub multiplier: u32,
    /// Ceiling on any single backoff delay.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    /// 4 retries with 1s → 2s → 4s → 8s backoff (last capped at `max_delay`),
    /// ~15s total before giving up. Sized for fast-returning transient errors
    /// (429/5xx), where the elapsed time *is* the backoff; timeout failures are
    /// bounded separately by the client's per-request timeout.
    fn default() -> Self {
        Self {
            max_retries: 4,
            base_delay: Duration::from_secs(1),
            multiplier: 2,
            max_delay: Duration::from_secs(8),
        }
    }
}

impl RetryPolicy {
    /// Backoff before the `attempt`-th retry (1-based: `attempt == 1` is the
    /// first retry). `base_delay * multiplier^(attempt - 1)`, saturating at
    /// `max_delay` so an overflowing factor can't wrap.
    fn delay_for(&self, attempt: u32) -> Duration {
        let factor = self.multiplier.saturating_pow(attempt.saturating_sub(1));
        self.base_delay.saturating_mul(factor).min(self.max_delay)
    }
}

/// Whether `err` is a transient failure worth another attempt.
///
/// Retries transport timeouts/connection drops and the provider's retryable
/// status codes — request timeout (408), rate limit (429), and gateway/server
/// errors (500, 502, 503, 504). Deterministic failures — other 4xx (bad
/// request, auth), malformed JSON, and request/response projection errors —
/// never retry, since the identical request would fail identically.
fn is_retryable(err: &CompletionError) -> bool {
    match err {
        CompletionError::HttpError(e) => e.is_timeout() || e.is_connect() || e.is_request(),
        CompletionError::ApiError { status, .. } => {
            matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
        }
        CompletionError::JsonError(_)
        | CompletionError::ResponseError(_)
        | CompletionError::RequestError(_) => false,
    }
}

/// Wraps a [`CompletionModel`], retrying transient
/// [`completion`](CompletionModel::completion) failures per a [`RetryPolicy`].
///
/// [`stream`](CompletionModel::stream) is delegated untouched: retrying a
/// half-consumed stream is a distinct problem left for the streaming work.
#[derive(Clone)]
pub struct RetryModel<M> {
    inner: M,
    policy: RetryPolicy,
}

impl<M> RetryModel<M> {
    /// Wraps `inner` with an explicit retry policy.
    pub fn with_policy(inner: M, policy: RetryPolicy) -> Self {
        Self { inner, policy }
    }
}

impl<M: CompletionModel> CompletionModel for RetryModel<M> {
    type Response = M::Response;
    type Client = M::Client;

    /// Builds the inner model via [`M::make`](CompletionModel::make) and wraps it
    /// with the [`Default`] policy. Use [`with_policy`](Self::with_policy) to
    /// supply a custom one.
    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        Self::with_policy(M::make(client, model), RetryPolicy::default())
    }

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        // `completion` consumes the request, so each retry needs a fresh clone.
        // The final attempt below reuses the owned `request`, so a zero-retry
        // policy clones nothing.
        for attempt in 0..self.policy.max_retries {
            match self.inner.completion(request.clone()).await {
                Ok(response) => return Ok(response),
                Err(err) if is_retryable(&err) => {
                    tokio::time::sleep(self.policy.delay_for(attempt + 1)).await;
                }
                Err(err) => return Err(err),
            }
        }
        self.inner.completion(request).await
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionStream, CompletionError> {
        self.inner.stream(request).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::completion::{AssistantContent, CompletionRequestBuilder, Message, Usage};
    use crate::non_empty_vec::NonEmptyVec;

    /// A model that returns scripted outcomes on successive `completion` calls
    /// and counts how many it received. Outcomes past the script default to
    /// success (so over-calling surfaces as a wrong count, not a panic).
    #[derive(Clone)]
    struct ScriptedModel {
        outcomes: std::sync::Arc<Mutex<VecDeque<Result<(), CompletionError>>>>,
        calls: std::sync::Arc<AtomicUsize>,
    }

    impl ScriptedModel {
        fn new(outcomes: Vec<Result<(), CompletionError>>) -> Self {
            Self {
                outcomes: std::sync::Arc::new(Mutex::new(outcomes.into())),
                calls: std::sync::Arc::new(AtomicUsize::new(0)),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    fn ok_response() -> CompletionResponse<()> {
        CompletionResponse {
            choice: NonEmptyVec::new(AssistantContent::text("ok")),
            usage: Usage::default(),
            raw_response: (),
            message_id: None,
        }
    }

    impl CompletionModel for ScriptedModel {
        type Response = ();
        type Client = ();

        fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
            unimplemented!("tests construct ScriptedModel directly")
        }

        async fn completion(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse<()>, CompletionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Guard dropped before any await, keeping the future `Send`.
            let next = self.outcomes.lock().expect("mutex").pop_front();
            match next {
                Some(Ok(())) | None => Ok(ok_response()),
                Some(Err(err)) => Err(err),
            }
        }

        async fn stream(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionStream, CompletionError> {
            unimplemented!("retry tests never stream")
        }
    }

    /// A policy with the given retry count and no actual waiting.
    fn instant_policy(max_retries: u32) -> RetryPolicy {
        RetryPolicy {
            max_retries,
            base_delay: Duration::ZERO,
            multiplier: 2,
            max_delay: Duration::ZERO,
        }
    }

    fn request() -> CompletionRequest {
        CompletionRequestBuilder::new(Message::user("hi")).build()
    }

    fn api_error(status: u16) -> CompletionError {
        CompletionError::ApiError {
            status,
            message: "boom".to_string(),
        }
    }

    #[tokio::test]
    async fn retries_transient_errors_then_succeeds() {
        let inner = ScriptedModel::new(vec![Err(api_error(503)), Err(api_error(429)), Ok(())]);
        let model = RetryModel::with_policy(inner.clone(), instant_policy(3));

        let result = model.completion(request()).await;

        assert!(result.is_ok(), "should recover after two transient errors");
        assert_eq!(inner.calls(), 3, "two failed attempts then one success");
    }

    #[tokio::test]
    async fn gives_up_after_max_retries_returning_the_last_error() {
        let inner = ScriptedModel::new(vec![
            Err(api_error(500)),
            Err(api_error(500)),
            Err(api_error(500)),
            Err(api_error(500)),
        ]);
        let model = RetryModel::with_policy(inner.clone(), instant_policy(3));

        let err = model
            .completion(request())
            .await
            .expect_err("all attempts fail");

        assert!(matches!(err, CompletionError::ApiError { status: 500, .. }));
        assert_eq!(inner.calls(), 4, "initial attempt plus three retries");
    }

    #[tokio::test]
    async fn does_not_retry_permanent_errors() {
        let inner = ScriptedModel::new(vec![Err(api_error(400))]);
        let model = RetryModel::with_policy(inner.clone(), instant_policy(3));

        let err = model
            .completion(request())
            .await
            .expect_err("permanent failure");

        assert!(matches!(err, CompletionError::ApiError { status: 400, .. }));
        assert_eq!(inner.calls(), 1, "a 4xx must not be retried");
    }

    #[tokio::test]
    async fn zero_retries_makes_a_single_attempt() {
        let inner = ScriptedModel::new(vec![Err(api_error(503))]);
        let model = RetryModel::with_policy(inner.clone(), instant_policy(0));

        let err = model
            .completion(request())
            .await
            .expect_err("no retries left");

        assert!(matches!(err, CompletionError::ApiError { status: 503, .. }));
        assert_eq!(inner.calls(), 1, "max_retries=0 means exactly one attempt");
    }

    #[test]
    fn is_retryable_classifies_status_codes_and_kinds() {
        for status in [408, 429, 500, 502, 503, 504] {
            assert!(is_retryable(&api_error(status)), "{status} should retry");
        }
        for status in [400, 401, 403, 404, 422, 501] {
            assert!(
                !is_retryable(&api_error(status)),
                "{status} should not retry"
            );
        }
        assert!(!is_retryable(&CompletionError::ResponseError("x".into())));
        assert!(!is_retryable(&CompletionError::RequestError("x".into())));
    }

    #[test]
    fn delay_grows_geometrically_and_caps() {
        let policy = RetryPolicy {
            max_retries: 9,
            base_delay: Duration::from_secs(1),
            multiplier: 2,
            max_delay: Duration::from_secs(8),
        };
        assert_eq!(policy.delay_for(1), Duration::from_secs(1));
        assert_eq!(policy.delay_for(2), Duration::from_secs(2));
        assert_eq!(policy.delay_for(3), Duration::from_secs(4));
        assert_eq!(policy.delay_for(4), Duration::from_secs(8));
        assert_eq!(
            policy.delay_for(5),
            Duration::from_secs(8),
            "capped at max_delay"
        );
    }
}
