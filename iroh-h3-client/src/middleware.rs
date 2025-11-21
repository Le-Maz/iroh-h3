//! Middleware and Service Abstractions for HTTP/3 Clients
//!
//! This `middleware` module provides traits and utilities for building composable,
//! dynamic HTTP services and middleware for use with `IrohH3Client`.
//!
//! ## Overview
//!
//! - `Service`: Basic request → response async handler.
//! - `Middleware`: Wraps a `Service` and can modify requests or responses.
//! - `Pipeline`: A dynamic boxed service used inside `IrohH3Client`.

pub mod follow_redirects;

use futures::future::BoxFuture;
use http::{Request, Response};
use std::future::Future;
use std::sync::Arc;

use crate::{body::Body, error::Error};

/// A generic HTTP service trait.
///
/// Services receive an HTTP request and produce a future resolving to an HTTP response or an error.
/// This is similar to Tower's `Service` trait, but simplified for `&self` usage and dynamic composition.
pub trait Service: Send + Sync {
    /// Handles a request asynchronously.
    ///
    /// # Parameters
    /// - `request`: The HTTP request to handle.
    ///
    /// # Returns
    /// A future resolving to either a response or an error.
    fn handle(
        &self,
        request: Request<Body>,
    ) -> impl Future<Output = Result<Response<Body>, Error>> + Send;
}

/// A middleware trait for HTTP services.
pub trait Middleware: Send + Sync {
    /// Handles a request, potentially delegating to the next service in the chain.
    fn handle(
        &self,
        request: Request<Body>,
        next: &impl Service,
    ) -> impl Future<Output = Result<Response<Body>, Error>> + Send;
}

/// Allows combining middleware and a service into a single service.
///
/// `(mw, svc).handle(req)` means:
/// - The middleware receives the request first
/// - It may inspect/modify the request
/// - It delegates to the service
impl<M, S> Service for (&M, &S)
where
    M: Middleware,
    S: Service,
{
    fn handle(
        &self,
        request: Request<Body>,
    ) -> impl Future<Output = Result<Response<Body>, Error>> + Send {
        self.0.handle(request, self.1)
    }
}

/// Implements left-to-right middleware composition:
///
/// `(mw1, mw2)` means:
///
/// ```text
/// request → mw1 → mw2 → next
/// ```
impl<M1, M2> Middleware for (M1, M2)
where
    M1: Middleware,
    M2: Middleware,
{
    async fn handle(
        &self,
        request: Request<Body>,
        next: &impl Service,
    ) -> Result<Response<Body>, Error> {
        // Compose mw2 + next into a temporary service
        let composed = (&self.1, next);

        // First call mw1, with mw2 as its "next"
        self.0.handle(request, &composed).await
    }
}

type ServiceFuture = BoxFuture<'static, Result<Response<Body>, Error>>;

/// A dynamic, thread-safe service pipeline.
pub struct Pipeline {
    inner: Box<dyn Fn(Request<Body>) -> ServiceFuture + Send + Sync>,
}

impl Pipeline {
    /// Creates a new `Pipeline` from any service implementing `Service`.
    pub fn new(service: impl Service + 'static) -> Self {
        let arc = Arc::new(service);
        Self {
            inner: Box::new(move |request| {
                let clone = arc.clone();
                Box::pin(async move { clone.handle(request).await })
            }),
        }
    }

    /// Creates a new `Pipeline` from a middleware + service pair.
    ///
    /// This constructs a dynamic service pipeline where the given `middleware`
    /// wraps the provided `service`. All requests sent through the resulting
    /// `Pipeline` will first pass through the middleware before reaching the
    /// underlying service.
    pub fn with_middleware(
        middleware: impl Middleware + 'static,
        service: impl Service + 'static,
    ) -> Self {
        let middleware_arc = Arc::new(middleware);
        let service_arc = Arc::new(service);
        Self {
            inner: Box::new(move |request| {
                let middleware_clone = middleware_arc.clone();
                let service_clone = service_arc.clone();
                Box::pin(async move {
                    let service = (middleware_clone.as_ref(), service_clone.as_ref());
                    service.handle(request).await
                })
            }),
        }
    }
}

impl Service for Pipeline {
    fn handle(
        &self,
        request: Request<Body>,
    ) -> impl Future<Output = Result<Response<Body>, Error>> + Send {
        (self.inner)(request)
    }
}
