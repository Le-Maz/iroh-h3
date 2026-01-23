use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Buf;
use bytes::Bytes;
use dashmap::{DashMap, Entry};
use futures::FutureExt;
use futures::future::Shared;
use h3::client::Connection;
use h3::client::RequestStream;
use http::Response;
use http::Uri;
use http::Version;
use http_body_util::BodyExt;
use iroh::{Endpoint, EndpointId};
use iroh_h3::BidiStream;
use iroh_h3::{Connection as IrohH3Connection, OpenStreams};
use n0_future::task; // unifies wasm/tokio task spawning.
use tracing::instrument;
use tracing::trace;
use tracing::warn;

use crate::body::Body;
use crate::error::Error;
use crate::error::RequestValidationError;
use crate::middleware::Service;
use crate::response::IrohH3ResponseBody;

type Sender = h3::client::SendRequest<OpenStreams, Bytes>;
type SenderFuture = Pin<Box<dyn futures::Future<Output = Result<Sender, Arc<Error>>> + Send>>;
type CachedSender = Shared<SenderFuture>;

#[derive(Clone, Debug)]
pub struct ConnectionManager {
    endpoint: Endpoint,
    alpn: Vec<u8>,
    sender_cache: Arc<DashMap<EndpointId, CachedSender>>,
}

impl ConnectionManager {
    pub fn new(endpoint: Endpoint, alpn: Vec<u8>) -> Self {
        Self {
            endpoint,
            alpn,
            sender_cache: Default::default(),
        }
    }

    #[instrument(skip(self, peer_id))]
    pub async fn get_sender(&self, peer_id: EndpointId) -> Result<Sender, Error> {
        // Try cached sender first
        if let Some(sender) = self.try_get_cached_sender(peer_id).await {
            return Ok(sender);
        }
        self.coordinate_connection_setup(peer_id).await
    }

    #[instrument(skip(self, peer_id))]
    async fn try_get_cached_sender(&self, peer_id: EndpointId) -> Option<Sender> {
        let cached_sender = self.sender_cache.get(&peer_id).as_deref().cloned();
        if let Some(shared) = cached_sender
            && let Ok(sender) = shared.await
        {
            return Some(sender);
        }
        None
    }

    #[instrument(skip(self, peer_id))]
    async fn coordinate_connection_setup(&self, peer_id: EndpointId) -> Result<Sender, Error> {
        loop {
            let action = {
                let entry = self.sender_cache.entry(peer_id);
                if let Entry::Occupied(sender_future) = entry {
                    ControlFlow::Continue(sender_future.get().clone())
                } else {
                    trace!("trying to connect to {}", peer_id.fmt_short());
                    let future = self.create_connection(peer_id);
                    entry.insert(future.clone());
                    ControlFlow::Break(future)
                }
            };

            match action {
                ControlFlow::Continue(shared) => {
                    if let Ok(sender) = shared.await {
                        return Ok(sender);
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                ControlFlow::Break(shared) => {
                    return match shared.await {
                        Ok(sender) => Ok(sender),
                        Err(err) => {
                            self.sender_cache.remove(&peer_id);
                            Err(err.into())
                        }
                    };
                }
            }
        }
    }

    #[instrument(skip(self, peer_id))]
    fn create_connection(&self, peer_id: EndpointId) -> Shared<SenderFuture> {
        let self_clone = self.clone();
        let fut: SenderFuture = Box::pin(async move {
            let conn = self_clone
                .endpoint
                .connect(peer_id, &self_clone.alpn)
                .await
                .map_err(|err| Error::Transport(err.into()))
                .map_err(Arc::new)?;
            let conn = IrohH3Connection::new(conn);
            let (conn, sender) = h3::client::new(conn)
                .await
                .map_err(|err| Error::Transport(err.into()))
                .map_err(Arc::new)?;

            // Cleanup task when connection closes
            task::spawn(self_clone.run_connection(conn, peer_id));

            Ok(sender)
        });

        fut.shared()
    }

    #[instrument(skip(self, conn))]
    async fn run_connection(
        self,
        mut conn: Connection<iroh_h3::Connection, Bytes>,
        peer_id: EndpointId,
    ) {
        let error = conn.wait_idle().await;
        trace!(
            "Connection with {} closed. Cause: {error}",
            peer_id.fmt_short()
        );
        self.sender_cache.remove(&peer_id);
    }

    /// Sends an HTTP body over the given request stream.
    ///
    /// Consumes all frames emitted by the provided [`Body`] and transmits them
    /// as HTTP/3 DATA or TRAILERS frames.
    #[instrument(skip(stream, body))]
    async fn send_body(
        stream: &mut RequestStream<BidiStream<Bytes>, Bytes>,
        body: Body,
    ) -> Result<(), Error> {
        let mut body_stream = body.into_stream();
        loop {
            match body_stream.frame().await.transpose()? {
                Some(frame) if frame.is_data() => {
                    let mut data = frame
                        .into_data()
                        .expect("Non-data frame in a branch guarded by is_data");
                    let buf = data.copy_to_bytes(data.remaining());
                    stream
                        .send_data(buf)
                        .await
                        .map_err(|err| Error::Transport(err.into()))?;
                }
                Some(frame) if frame.is_trailers() => {
                    let trailers = frame
                        .into_trailers()
                        .expect("Non-trailers frame in a branch guarded by is_trailers");
                    stream
                        .send_trailers(trailers)
                        .await
                        .map_err(|err| Error::Transport(err.into()))?;
                }
                Some(_) => warn!("Unexpected frame type"),
                None => break,
            }
        }
        Ok(())
    }
}

impl Service for ConnectionManager {
    #[instrument(skip(self, request))]
    async fn handle(&self, mut request: http::Request<Body>) -> Result<Response<Body>, Error> {
        let peer_id = peer_id(request.uri())?;
        let mut sender = self.get_sender(peer_id).await?;

        *request.version_mut() = Version::HTTP_3;

        let (parts, body) = request.into_parts();
        let req = http::Request::from_parts(parts, ());

        let mut stream = sender
            .send_request(req)
            .await
            .map_err(|err| Error::Transport(err.into()))?;
        Self::send_body(&mut stream, body).await?;
        stream
            .finish()
            .await
            .map_err(|err| Error::Transport(err.into()))?;

        let response = stream
            .recv_response()
            .await
            .map_err(|err| Error::Transport(err.into()))?;
        let inner = response.into_parts().0;

        let response_body = IrohH3ResponseBody::new(stream, sender);
        let boxed_response_body = response_body.boxed();

        Ok(Response::from_parts(inner, boxed_response_body.into()))
    }
}

/// Extracts the [`EndpointId`] from the authority component of a URI.
///
/// # Errors
///
/// Returns:
/// - [`Error::MissingAuthority`] if the URI lacks an authority.
/// - [`Error::BadPeerId`] if the authority is not a valid [`EndpointId`].
#[instrument]
pub(crate) fn peer_id(uri: &Uri) -> Result<EndpointId, Error> {
    let authority = uri
        .authority()
        .ok_or_else(|| RequestValidationError::MissingAuthority)?
        .as_str();
    authority
        .parse()
        .map_err(|err| RequestValidationError::BadPeerId(err).into())
}
