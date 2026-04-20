//! SmartTimeout middleware for IrohH3Client
//!
//! Path-aware timeout middleware that exempts certain URI prefixes from timeout enforcement.
//! Useful for long-running requests like audio streaming that should not be subject to
//! the same timeout as short API calls.
//!
//! # Example
//! ```rust,no_run
//! use iroh_h3_client::middleware::smart_timeout::SmartTimeout;
//! use std::time::Duration;
//!
//! let timeout = SmartTimeout::new(Duration::from_secs(3))
//!     .exempt_prefix("/api/v1/xxx_stream");
//! ```

use crate::{
    body::Body,
    error::{Error, MiddlewareError},
    middleware::{Middleware, Service},
};
use http::{Request, Response};
use n0_future::time;
use std::time::Duration;
use tracing::{debug, instrument};

/// Middleware that applies a timeout to requests, with path-based exemptions.
///
/// Requests whose URI path starts with any of the configured exempt prefixes
/// are passed through without a timeout. All other requests are subject to
/// the configured duration limit.
pub struct SmartTimeout {
    duration: Duration,
    exempt_prefixes: Vec<String>,
}

impl SmartTimeout {
    /// Construct a new SmartTimeout middleware with the given duration.
    pub fn new(duration: Duration) -> Self {
        Self {
            duration,
            exempt_prefixes: Vec::new(),
        }
    }

    /// Add a URI path prefix that should be exempt from timeout enforcement.
    pub fn exempt_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.exempt_prefixes.push(prefix.into());
        self
    }
}

impl Middleware for SmartTimeout {
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
        let path = request.uri().path();

        if self
            .exempt_prefixes
            .iter()
            .any(|prefix| path.starts_with(prefix))
        {
            debug!("exempt from timeout, passing through");
            return next.handle(request).await;
        }

        debug!("sending request with timeout");
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

    struct MockService {
        results: Arc<Mutex<Vec<Result<Response<Body>, Error>>>>,
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

    #[tokio::test]
    async fn normal_request_times_out() {
        let service = MockService::new(vec![Ok(ok_response())], 200);
        let mw = SmartTimeout::new(Duration::from_millis(50));
        let req = Request::builder()
            .uri("/api/v1/ping")
            .body(Body::empty())
            .unwrap();

        let result = mw.handle(req, &service).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn normal_request_succeeds_within_timeout() {
        let service = MockService::new(vec![Ok(ok_response())], 10);
        let mw = SmartTimeout::new(Duration::from_millis(200));
        let req = Request::builder()
            .uri("/api/v1/ping")
            .body(Body::empty())
            .unwrap();

        let result = mw.handle(req, &service).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn exempt_prefix_bypasses_timeout() {
        let service = MockService::new(vec![Ok(ok_response())], 200);
        let mw = SmartTimeout::new(Duration::from_millis(50))
            .exempt_prefix("/api/v1/xxx_stream");
        let req = Request::builder()
            .uri("/api/v1/xxx_stream?aaa=abc")
            .body(Body::empty())
            .unwrap();

        // Even though delay (200ms) > timeout (50ms), exempt prefix should bypass
        let result = mw.handle(req, &service).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn non_matching_prefix_still_times_out() {
        let service = MockService::new(vec![Ok(ok_response())], 200);
        let mw = SmartTimeout::new(Duration::from_millis(50))
            .exempt_prefix("/api/v1/xxx_stream");
        let req = Request::builder()
            .uri("/api/v1/get_library")
            .body(Body::empty())
            .unwrap();

        let result = mw.handle(req, &service).await;
        assert!(result.is_err());
    }
}
