//! # Iroh HTTP/3 Client
//!
//! `iroh-h3-client` is a Rust library for building and sending HTTP/3 requests using the QUIC-based
//! HTTP/3 protocol. It provides a high-level, ergonomic API for constructing requests, setting
//! headers and body content, and receiving responses asynchronously.  
//!
//! ## Features
//!
//! - **HTTP/3 Requests:** Build requests with custom headers, extensions, and bodies.
//! - **Connection Reuse:** Connections are automatically reused across multiple requests to the same server, improving performance.
//! - **Flexible Body Types:** Send plain text, binary data, or JSON-serialized payloads.
//! - **Streaming Responses:** Read response bodies incrementally without buffering the entire content.
//! - **Full Body Consumption:** Convenient methods to read response bodies as `Bytes` or `String`.
//! - **JSON Integration:** (Optional) Serialize request bodies and deserialize response bodies using `serde`.
//!
//! ## Optional Features
//!
//! - `json` — Enables `RequestBuilder::json` and `Response::json` for working with JSON payloads.
//!
//! ## Error Handling
//!
//! All operations return a crate-specific [`Error`] type that can represent connection, stream,
//! serialization, or protocol-level errors.

#![warn(missing_docs)]

mod body;
mod connection_manager;
pub mod error;
pub mod middleware;
pub mod request;
pub mod response;

use std::fmt::Debug;
use std::sync::Arc;

use http::request::Builder;
use http::{Method, Uri};
use iroh::Endpoint;

use crate::body::Body;
use crate::connection_manager::ConnectionManager;
use crate::error::Error;
use crate::middleware::{Middleware, Pipeline, Service};
use crate::request::RequestBuilder;

/// An HTTP/3 client built on top of an [`iroh`] QUIC [`Endpoint`].
///
/// The [`IrohH3Client`] handles the setup and management of QUIC + HTTP/3
/// connections between peers in the Iroh network. It provides a simple interface
/// for building and sending requests using standard [`http`] types.
///
/// # Overview
///
/// - Manages connection pooling and caching using peer [`EndpointId`]s.
/// - Provides ergonomic builders for all standard HTTP methods.
/// - Supports streaming request and response bodies.
///
/// # Example
///
/// ```rust
/// use bytes::Bytes;
/// use http::Request;
/// use iroh::{Endpoint, EndpointId};
/// use iroh_h3_client::IrohH3Client;
///
/// # async fn example(peer_id: EndpointId) -> Result<(), Box<dyn std::error::Error>> {
/// let endpoint = Endpoint::bind().await?;
/// let client = IrohH3Client::new(endpoint, b"h3".to_vec());
///
/// let url = format!("iroh+h3://{peer_id}/hello");
/// let request = client.get(&url).text("hello")?;
///
/// let response = request.send().await?;
/// let body = response.bytes().await?;
/// println!("Response: {:?}", body);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
#[repr(transparent)]
pub struct IrohH3Client {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    service: Pipeline,
}

impl Debug for ClientInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientInner")
            .field("service", &"...")
            .finish()
    }
}

macro_rules! http_method {
    ($name:ident, $variant:expr) => {
        #[inline]
        /// Begins building a new request using the HTTP `$variant` method.
        ///
        /// This is equivalent to calling [`IrohH3Client::request`] with the
        /// corresponding [`Method`].
        pub fn $name<U>(&self, uri: U) -> RequestBuilder
        where
            U: TryInto<Uri>,
            http::Error: From<<U as TryInto<Uri>>::Error>,
        {
            self.request($variant, uri)
        }
    };
}

impl IrohH3Client {
    /// Creates a new [`IrohH3Client`] using the provided QUIC [`Endpoint`] and ALPN identifier.
    ///
    /// The ALPN string is used during QUIC connection negotiation to select the
    /// HTTP/3 protocol variant.
    pub fn new(endpoint: Endpoint, alpn: Vec<u8>) -> Self {
        let connection_manager = ConnectionManager::new(endpoint, alpn);
        let inner = ClientInner {
            service: Pipeline::new(connection_manager),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Creates a new `IrohH3Client` with a middleware layer applied to all requests.
    ///
    /// This constructor wraps the base HTTP/3 connection manager with the provided
    /// middleware, allowing interception, logging, modification, or other behaviors
    /// for every request sent through this client.
    ///
    /// # Parameters
    /// - `endpoint`: The QUIC `Endpoint` to use for connections.
    /// - `alpn`: The ALPN identifier (e.g., `b"h3"`) used during QUIC negotiation.
    /// - `middleware`: An implementation of the `Middleware` trait to wrap the client.
    ///
    /// # Returns
    /// A new instance of `IrohH3Client` where all requests pass through the middleware.
    pub fn with_middleware(
        endpoint: Endpoint,
        alpn: Vec<u8>,
        middleware: impl Middleware + 'static,
    ) -> Self {
        let service = Pipeline::with_middleware(middleware, ConnectionManager::new(endpoint, alpn));
        let inner = ClientInner { service };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Creates a new [`RequestBuilder`] associated with this client.
    ///
    /// This is the generic entry point for constructing an HTTP/3 request with
    /// a custom method.
    ///
    /// # Example
    /// ```rust
    /// use iroh_h3_client::IrohH3Client;
    /// # async fn example(client: IrohH3Client) {
    /// let req = client.request(http::Method::PUT, "iroh+h3://peer/some/path").build();
    /// # }
    /// ```
    pub fn request<U>(&self, method: http::Method, uri: U) -> RequestBuilder
    where
        U: TryInto<Uri>,
        http::Error: From<<U as TryInto<Uri>>::Error>,
    {
        RequestBuilder {
            inner: Builder::new().method(method).uri(uri),
            client: self.clone(),
        }
    }

    http_method!(head, Method::HEAD);
    http_method!(get, Method::GET);
    http_method!(post, Method::POST);
    http_method!(put, Method::PUT);
    http_method!(patch, Method::PATCH);
    http_method!(delete, Method::DELETE);

    /// Sends an HTTP/3 request and awaits the response.
    #[tracing::instrument(skip(self))]
    async fn send(&self, request: http::Request<Body>) -> Result<crate::response::Response, Error> {
        let response = self.inner.service.handle(request).await?;
        let (inner, body) = response.into_parts();
        Ok(crate::response::Response { inner, body })
    }
}
