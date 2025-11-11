use std::{
    error::Error,
    pin::Pin,
    task::{Context, Poll},
};

use axum::{Router, body::HttpBody};
use bytes::{Buf, Bytes};
use futures::StreamExt;
use h3::server::{self, RequestStream};
use http::{Request, Response};
use http_body::Frame;
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh_h3::{BidiStream, Connection as IrohH3Connection, RecvStream};
use tower_service::Service;

/// Type alias for the H3 server-side connection using iroh QUIC transport.
type H3ServerConnection = server::Connection<IrohH3Connection, Bytes>;

/// Type alias for an HTTP/3 bidirectional request stream over iroh QUIC.
type H3ServerRequestStream = server::RequestStream<BidiStream<Bytes>, Bytes>;

/// An HTTP/3 protocol handler that serves an [`axum::Router`] over iroh.
///
/// This integrates the Axum web framework with the [`iroh`] QUIC transport,
/// allowing HTTP/3 requests to be handled through your Axum routes.
#[derive(Debug)]
pub struct IrohAxum {
    router: Router,
}

impl IrohAxum {
    /// Creates a new [`IrohAxum`] server from an [`axum::Router`].
    pub fn new(router: Router) -> Self {
        Self { router }
    }

    /// Handles a single HTTP/3 request stream by routing it through Axum.
    ///
    /// Spawns a new asynchronous task that:
    /// - Wraps the incoming QUIC stream as an Axum-compatible body.
    /// - Calls the [`Router`] service to obtain a response.
    /// - Streams the response body back over the QUIC stream.
    fn handle_request(&self, request: http::Request<()>, stream: H3ServerRequestStream) {
        let router = self.router.clone();

        tokio::spawn(async move {
            let mut router = router;

            // Extract request parts (headers, method, etc.).
            let parts = request.into_parts().0;

            // Split the bidirectional stream into sender and receiver halves.
            let (mut send, recv) = stream.split();

            // Wrap the receive half in an Axum-compatible body.
            let request_body = RequestBody { inner: recv };
            let request = Request::from_parts(parts, request_body);

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
    async fn accept(&self, connection: iroh::endpoint::Connection) -> Result<(), AcceptError> {
        // Wrap the raw iroh connection in an HTTP/3 server connection.
        let connection = IrohH3Connection::new(connection);
        let mut connection = H3ServerConnection::new(connection)
            .await
            .map_err(AcceptError::from_err)?;

        // Accept and process all incoming requests on this connection.
        while let Some(request_resolver) =
            connection.accept().await.map_err(AcceptError::from_err)?
        {
            let (request, stream) = request_resolver
                .resolve_request()
                .await
                .map_err(AcceptError::from_err)?;

            self.handle_request(request, stream);
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
