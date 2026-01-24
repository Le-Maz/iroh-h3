//! RetryFailures middleware for IrohH3Client
//!
//! Automatically retries failed HTTP requests up to a configured limit.
//! - Retries on transport errors (`Error`) or non-success HTTP statuses (except 410 Gone)
//! - Stops retrying if a response is 410 Gone or a request eventually succeeds
//! - Uses **exponential backoff** between retries (`base_delay_ms * 2^(attempts-1)`)
//!
//! # Behavior
//!
//! - **Retryable conditions**:
//!     - Transport or network errors
//!     - Non-success HTTP status codes (except 410 Gone)
//! - **Non-retry conditions**:
//!     - 2xx successful responses
//!     - 410 Gone responses
//! - **Body handling**: The request body is replaced with `Body::empty()` for retries to avoid replay issues.

use crate::{
    body::Body,
    error::{Error, MiddlewareError},
    middleware::{Middleware, Service},
};
use http::{Request, Response, StatusCode};
use n0_future::time; // unifies wasm/tokio task spawning.
use std::ops::ControlFlow;
use tracing::{debug, instrument, warn};

/// Middleware that automatically retries failed requests with exponential backoff.
pub struct RetryFailures {
    /// Maximum number of retry attempts.
    pub max_retries: usize,

    /// Base delay in milliseconds for exponential backoff.
    /// Actual delay for retry `n` = `base_delay_ms * 2^(n-1)`.
    pub base_delay_ms: u64,
}

impl RetryFailures {
    /// Construct a new RetryFailures middleware.
    pub fn new(max_retries: usize, base_delay_ms: u64) -> Self {
        Self {
            max_retries,
            base_delay_ms,
        }
    }

    /// Perform one retry step.
    #[instrument(
        skip(self, parts, body_slot, attempts, next),
        fields(attempts = *attempts)
    )]
    async fn retry_step(
        &self,
        parts: &mut http::request::Parts,
        body_slot: &mut Body,
        attempts: &mut usize,
        next: &impl Service,
    ) -> Result<ControlFlow<Response<Body>>, Error> {
        debug!("sending request");

        let req = Request::from_parts(parts.clone(), body_slot.take());
        let result = next.handle(req).await;

        match result {
            Ok(response) => {
                let status = response.status();
                debug!(%status, "received response");

                if status == StatusCode::GONE {
                    debug!("received 410 Gone — not retrying");
                    return Ok(ControlFlow::Break(response));
                }

                if status.is_success() {
                    debug!("response successful");
                    return Ok(ControlFlow::Break(response));
                }

                // Non-success status != 410 → retry
                *attempts += 1;
                warn!(attempts = *attempts, %status, "HTTP error; retrying");

                if *attempts > self.max_retries {
                    warn!("too many retries ({})", self.max_retries);
                    return Ok(ControlFlow::Break(response));
                }

                *body_slot = Body::empty();

                if self.base_delay_ms > 0 {
                    let delay = self.base_delay_ms * 2_u64.pow((*attempts - 1) as u32);
                    debug!(ms = delay, "waiting before next retry");
                    time::sleep(std::time::Duration::from_millis(delay)).await;
                }

                Ok(ControlFlow::Continue(()))
            }

            Err(err) => {
                *attempts += 1;
                warn!(attempts = *attempts, error = ?err, "transport error; retrying");

                if *attempts > self.max_retries {
                    warn!("too many retries ({})", self.max_retries);
                    return Err(MiddlewareError::RetryLimitExceeded(Box::new(err)).into());
                }

                *body_slot = Body::empty();

                if self.base_delay_ms > 0 {
                    let delay = self.base_delay_ms * 2_u64.pow((*attempts - 1) as u32);
                    debug!(ms = delay, "waiting before next retry");
                    time::sleep(std::time::Duration::from_millis(delay)).await;
                }

                Ok(ControlFlow::Continue(()))
            }
        }
    }
}

impl Middleware for RetryFailures {
    #[instrument(
        skip(self, next, request),
        fields(
            method = %request.method(),
            uri = %request.uri(),
            max_retries = self.max_retries
        )
    )]
    async fn handle(
        &self,
        request: Request<Body>,
        next: &impl Service,
    ) -> Result<Response<Body>, Error> {
        let (mut parts, mut body) = request.into_parts();
        let mut attempts = 0;

        loop {
            match self
                .retry_step(&mut parts, &mut body, &mut attempts, next)
                .await?
            {
                ControlFlow::Continue(_) => continue,
                ControlFlow::Break(resp) => return Ok(resp),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Response, StatusCode};
    use std::sync::{Arc, Mutex};

    struct MockService {
        results: Arc<Mutex<Vec<Result<Response<Body>, Error>>>>,
    }

    impl MockService {
        fn new(results: Vec<Result<Response<Body>, Error>>) -> Self {
            Self {
                results: Arc::new(Mutex::new(results)),
            }
        }
    }

    impl Service for MockService {
        async fn handle(&self, _req: Request<Body>) -> Result<Response<Body>, Error> {
            self.results.lock().unwrap().remove(0)
        }
    }

    fn ok_response() -> Response<Body> {
        Response::builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .unwrap()
    }

    fn status_response(code: StatusCode) -> Response<Body> {
        Response::builder()
            .status(code)
            .body(Body::empty())
            .unwrap()
    }

    fn err() -> Error {
        Error::Other("network failure".into())
    }

    #[tokio::test]
    async fn success_without_retry() {
        let service = MockService::new(vec![Ok(ok_response())]);
        let retry = RetryFailures::new(3, 0);
        let req = Request::new(Body::empty());

        let resp = retry.handle(req, &service).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn retries_then_succeeds() {
        let service = MockService::new(vec![
            Ok(status_response(StatusCode::INTERNAL_SERVER_ERROR)),
            Ok(ok_response()),
        ]);

        let retry = RetryFailures::new(5, 0);
        let req = Request::new(Body::empty());
        let resp = retry.handle(req, &service).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn retries_on_error_then_succeeds() {
        let service = MockService::new(vec![Err(err()), Err(err()), Ok(ok_response())]);

        let retry = RetryFailures::new(5, 0);
        let req = Request::new(Body::empty());
        let resp = retry.handle(req, &service).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn stops_on_gone_status() {
        let service = MockService::new(vec![
            Ok(status_response(StatusCode::GONE)),
            Ok(ok_response()),
        ]);

        let retry = RetryFailures::new(5, 0);
        let req = Request::new(Body::empty());
        let resp = retry.handle(req, &service).await.unwrap();

        assert_eq!(resp.status(), StatusCode::GONE);
    }

    #[tokio::test]
    async fn gives_up_after_max_retries() {
        let service = MockService::new(vec![Err(err()), Err(err()), Err(err())]);

        let retry = RetryFailures::new(2, 0);
        let req = Request::new(Body::empty());
        let result = retry.handle(req, &service).await;

        assert!(result.is_err());
    }
}
