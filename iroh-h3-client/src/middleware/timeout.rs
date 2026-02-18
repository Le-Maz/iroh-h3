//! Timeout middleware for IrohH3Client
//!
//! Automatically applies a timeout to each HTTP request.
//! - If the request does not complete within the specified duration, returns an `Error::Other`.
//! - Works for any request type and does not modify the request or response.

use crate::{
    body::Body,
    error::{Error, MiddlewareError},
    middleware::{Middleware, Service},
};
use http::{Request, Response};
use std::time::Duration;
use n0_future::time;
use tracing::{debug, instrument};

/// Middleware that applies a timeout to each request.
pub struct Timeout {
    /// Maximum allowed duration for a single request.
    pub duration: Duration,
}

impl Timeout {
    /// Construct a new Timeout middleware.
    pub fn new(duration: Duration) -> Self {
        Self { duration }
    }
}

impl Middleware for Timeout {
    #[instrument(
        skip(self, next, request),
        fields(
            method = %request.method(),
            uri = %request.uri(),
            timeout_ms = self.duration.as_millis()
        )
    )]
    async fn handle(
        &self,
        request: Request<Body>,
        next: &impl Service,
    ) -> Result<Response<Body>, Error> {
        debug!("sending request with timeout");

        // Wrap the service call in tokio timeout
        match time::timeout(self.duration, next.handle(request)).await {
            Ok(result) => result,
            Err(_) => Err(MiddlewareError::Timeout.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Response, StatusCode};
    use std::sync::{Arc, Mutex};

    type ResultsInner = Vec<Result<Response<Body>, Error>>;
    /// Mock service that can delay or return responses/errors in sequence.
    struct MockService {
        results: Arc<Mutex<ResultsInner>>,
        delay_ms: u64,
    }

    impl MockService {
        fn new(results: Vec<Result<Response<Body>, Error>>, delay_ms: u64) -> Self {
            Self {
                results: Arc::new(Mutex::new(results)),
                delay_ms,
            }
        }
    }

    impl Service for MockService {
        async fn handle(&self, _req: Request<Body>) -> Result<Response<Body>, Error> {
            if self.delay_ms > 0 {
                n0_future::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            self.results.lock().unwrap().remove(0)
        }
    }

    fn ok_response() -> Response<Body> {
        Response::builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .unwrap()
    }

    fn err() -> Error {
        Error::Other("network error".into())
    }

    #[tokio::test]
    async fn completes_within_timeout() {
        let service = MockService::new(vec![Ok(ok_response())], 10);
        let timeout = Timeout::new(Duration::from_millis(50));
        let req = Request::new(Body::empty());

        let resp = timeout.handle(req, &service).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn triggers_timeout_error() {
        let service = MockService::new(vec![Ok(ok_response())], 100);
        let timeout = Timeout::new(Duration::from_millis(10));
        let req = Request::new(Body::empty());

        let result = timeout.handle(req, &service).await;
        assert!(matches!(
            result,
            Err(Error::Middleware(MiddlewareError::Timeout))
        ));
    }

    #[tokio::test]
    async fn propagates_service_error() {
        let service = MockService::new(vec![Err(err())], 10);
        let timeout = Timeout::new(Duration::from_millis(50));
        let req = Request::new(Body::empty());

        let result = timeout.handle(req, &service).await;
        assert!(matches!(result, Err(Error::Other(_))));
    }
}
