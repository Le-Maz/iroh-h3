use std::{
    error::Error,
    pin::Pin,
    task::{Context, Poll},
};

use axum::{Router, body::HttpBody};
use bytes::{Buf, Bytes};
use futures::StreamExt;
use h3::server::RequestStream;
use http::{Request, Response};
use http_body::Frame;
use iroh::protocol::{AcceptError, ProtocolHandler};
use tower_service::Service;

use iroh_h3::{BidiStream, Connection, RecvStream};

type H3ServerConnection = h3::server::Connection<iroh_h3::Connection, Bytes>;
type H3ServerRequestStream = h3::server::RequestStream<BidiStream<Bytes>, Bytes>;

#[derive(Debug)]
pub struct IrohAxum {
    router: Router,
}

impl IrohAxum {
    pub fn new(router: Router) -> Self {
        Self { router }
    }
}

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
        let poll = self.get_mut().inner.poll_recv_data(cx);
        match poll {
            Poll::Ready(Ok(Some(mut chunk))) => {
                let chunk = chunk.copy_to_bytes(chunk.remaining());
                let frame = Frame::data(chunk);
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Ok(None)) => Poll::Ready(None),
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl IrohAxum {
    async fn handle_request(&self, request: http::Request<()>, stream: H3ServerRequestStream) {
        let router = self.router.clone();
        tokio::spawn(async move {
            let mut router = router;
            let parts = request.into_parts().0;
            let (mut send, recv) = stream.split();
            let request_body = RequestBody { inner: recv };
            let request = Request::from_parts(parts, request_body);

            let response = router.call(request).await?;

            let (parts, body) = response.into_parts();
            let response: Response<()> = Response::from_parts(parts, ());
            send.send_response(response).await?;
            let mut response_body_stream = body.into_data_stream();
            while let Some(Ok(chunk)) = response_body_stream.next().await {
                send.send_data(chunk).await?;
            }
            send.finish().await?;
            Ok::<(), Box<dyn Error + Send + Sync>>(())
        });
    }
}

impl ProtocolHandler for IrohAxum {
    async fn accept(
        &self,
        connection: iroh::endpoint::Connection,
    ) -> Result<(), iroh::protocol::AcceptError> {
        let connection = Connection::new(connection);
        let mut connection = H3ServerConnection::new(connection)
            .await
            .map_err(AcceptError::from_err)?;
        while let Some(request_resolver) =
            connection.accept().await.map_err(AcceptError::from_err)?
        {
            let (request, stream) = request_resolver
                .resolve_request()
                .await
                .map_err(AcceptError::from_err)?;
            self.handle_request(request, stream).await;
        }
        Ok(())
    }
}
