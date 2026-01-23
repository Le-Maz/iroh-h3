//! # iroh-h3-axum
//!
//! This crate provides an integration between the Axum web framework and the
//! iroh peer-to-peer library using HTTP/3.  
//! It allows you to serve Axum routers over iroh, reusing connections and
//! handling multiple concurrent HTTP/3 streams efficiently.
//!
//! ## Features
//! - Serve HTTP/3 endpoints using Axum routes
//! - Wrap QUIC streams as Axum-compatible request bodies
//! - Stream response bodies over HTTP/3
//! - Provide access to the remote iroh endpoint ID via request extensions
//!
//! [Axum]: https://docs.rs/axum
//! [iroh]: https://docs.rs/iroh

#![deny(missing_docs)]

use std::{
    error::Error,
    pin::Pin,
    task::{Context, Poll},
};

use axum::{Router, body::HttpBody, extract::FromRequestParts};
use bytes::{Buf, Bytes};
use futures_lite::StreamExt;
use h3::server::{self, RequestResolver, RequestStream};
use http::{Request, Response, StatusCode};
use http_body::Frame;
use iroh::{
    EndpointId,
    protocol::{AcceptError, ProtocolHandler},
};
use iroh_h3::{Connection as IrohH3Connection, RecvStream};
use n0_future::task; // unifies wasm/tokio task spawning.
use tower_service::Service;

/// Type alias for the HTTP/3 server-side connection using iroh QUIC transport.
type H3ServerConnection = server::Connection<IrohH3Connection, Bytes>;

/// An HTTP/3 protocol handler that serves an [`axum::Router`] over iroh.
///
/// This integrates the Axum web framework with the [`iroh`] QUIC transport,
/// allowing HTTP/3 requests to be handled through your Axum routes.
///
/// Connections are reused automatically by the underlying QUIC transport.
#[derive(Debug)]
pub struct IrohAxum {
    router: Router,
}

impl IrohAxum {
    /// Creates a new [`IrohAxum`] server from an [`axum::Router`].
    ///
    /// # Example
    /// ```rust
    /// use axum::Router;
    /// use iroh_h3_axum::IrohAxum;
    ///
    /// let router = Router::new();
    /// let server = IrohAxum::new(router);
    /// ```
    #[inline]
    pub fn new(router: Router) -> Self {
        Self { router }
    }

    /// Handles a single HTTP/3 request stream by routing it through Axum.
    ///
    /// This method:
    /// - Wraps the incoming QUIC stream as an Axum-compatible body
    /// - Calls the [`Router`] service to obtain a response
    /// - Streams the response body back over the QUIC connection
    fn handle_request(
        &self,
        remote_id: EndpointId,
        request_resolver: RequestResolver<IrohH3Connection, Bytes>,
    ) {
        let router = self.router.clone();

        task::spawn(async move {
            let mut router = router;

            let (request, stream) = request_resolver.resolve_request().await?;

            // Extract request parts (headers, method, etc.).
            let parts = request.into_parts().0;

            // Split the bidirectional stream into sender and receiver halves.
            let (mut send, recv) = stream.split();

            // Wrap the receive half in an Axum-compatible body.
            let request_body = RequestBody { inner: recv };
            let mut request = Request::from_parts(parts, request_body);
            request.extensions_mut().insert(RemoteId(remote_id));

            // Call into the Axum router.
            let response = router.call(request).await?;

            // Send response headers.
            let (parts, body) = response.into_parts();
            let response_head: Response<()> = Response::from_parts(parts, ());
            send.send_response(response_head).await?;

            // Stream response body frames.
            let mut response_stream = body.into_data_stream();
            while let Some(Ok(chunk)) = response_stream.next().await {
                send.send_data(chunk).await?;
            }

            // Gracefully finish the response.
            send.finish().await?;

            Ok::<(), Box<dyn Error + Send + Sync>>(())
        });
    }
}

/// Implements the [`ProtocolHandler`] trait so this can be registered
/// directly with an [`iroh::Endpoint`].
///
/// The handler listens for new QUIC connections, accepts incoming HTTP/3
/// requests, and dispatches them through the Axum router.
impl ProtocolHandler for IrohAxum {
    /// Accepts an incoming iroh QUIC connection and serves HTTP/3 requests.
    ///
    /// # Errors
    /// Returns [`AcceptError`] if connection initialization or request handling fails.
    async fn accept(&self, connection: iroh::endpoint::Connection) -> Result<(), AcceptError> {
        let remote_id = connection.remote_id();
        let connection = IrohH3Connection::new(connection);
        let mut connection = H3ServerConnection::new(connection)
            .await
            .map_err(AcceptError::from_err)?;

        while let Some(request_resolver) =
            connection.accept().await.map_err(AcceptError::from_err)?
        {
            self.handle_request(remote_id, request_resolver);
        }

        Ok(())
    }
}

/// Wrapper for an incoming HTTP/3 request body that implements [`HttpBody`]
/// so it can be used directly with Axum.
///
/// Converts QUIC data frames from the HTTP/3 [`RequestStream`] into
/// [`http_body::Frame`]s that Axum understands.
#[repr(transparent)]
struct RequestBody {
    inner: RequestStream<RecvStream, Bytes>,
}

impl HttpBody for RequestBody {
    type Data = Bytes;
    type Error = h3::error::StreamError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let inner = self.get_mut();
        match inner.inner.poll_recv_data(cx) {
            Poll::Ready(Ok(Some(mut chunk))) => {
                let bytes = chunk.copy_to_bytes(chunk.remaining());
                Poll::Ready(Some(Ok(Frame::data(bytes))))
            }
            Poll::Ready(Ok(None)) => Poll::Ready(None),
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// An Axum request extractor for the remote endpoint ID.
///
/// This type allows you to access the [`EndpointId`] of the client that
/// initiated the HTTP/3 request over iroh.
///
/// # Example
/// ```rust
/// use axum::{Router, routing::get};
/// use iroh_h3_axum::RemoteId;
///
/// async fn handler(RemoteId(remote_id): RemoteId) -> String {
///     format!("Hello there {:?}", remote_id)
/// }
///
/// let app = Router::<()>::new().route("/", get(handler));
/// ```
#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub struct RemoteId(pub EndpointId);

impl<S> FromRequestParts<S> for RemoteId
where
    S: Sync,
{
    type Rejection = (StatusCode, &'static str);

    /// Extracts the remote endpoint ID from the request extensions.
    ///
    /// # Errors
    /// Returns [`StatusCode::INTERNAL_SERVER_ERROR`] if used outside an iroh-h3-axum context.
    #[inline]
    async fn from_request_parts(
        parts: &mut http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts.extensions.get().copied().ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Trying to extract RemoteId outside iroh-h3-axum context",
        ))
    }
}
