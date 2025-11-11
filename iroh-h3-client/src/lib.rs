pub mod error;
pub mod request;
pub mod response;

use std::collections::HashMap;
use std::fmt::Debug;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use bytes::{Buf, Bytes};
use futures::future::Shared;
use futures::{FutureExt, StreamExt};
use h3::client::{RequestStream, SendRequest};
use http::request::Builder;
use http::{Method, Uri, Version};
use http_body::Body;
use http_body_util::BodyStream;
use iroh::{Endpoint, EndpointId};
use iroh_h3::{BidiStream, Connection as IrohH3Connection, OpenStreams};

use crate::error::Error;
use crate::request::RequestBuilder;
use crate::response::Response;

/// A client for sending HTTP/3 requests over an [`iroh`] QUIC endpoint.
///
/// This client wraps an [`iroh::Endpoint`] and handles the details of establishing
/// connections, performing ALPN negotiation, sending requests, and receiving responses.
///
/// # Example
///
/// ```rust,ignore
/// let endpoint = iroh::Endpoint::builder().bind().await?;
/// let client = IrohH3Client::new(endpoint, b"h3".to_vec());
///
/// let request = http::Request::builder()
///     .uri("iroh+h3://peer-id/some/path")
///     .body(Bytes::from("hello world"))?;
///
/// let mut response = client.send(request).await?;
/// let body = response.body_bytes().await?;
/// println!("Response body: {:?}", body);
/// ```
#[derive(Clone)]
#[repr(transparent)]
pub struct IrohH3Client {
    inner: Arc<ClientInner>,
}

impl Debug for IrohH3Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrohH3Client")
            .field("endpoint", &self.inner.endpoint)
            .field("alpn", &self.inner.alpn)
            .field("connections", &"...")
            .finish()
    }
}

struct ClientInner {
    endpoint: Endpoint,
    alpn: Vec<u8>,
    sender_cache: RwLock<HashMap<EndpointId, CachedSender>>,
}

type Sender = SendRequest<OpenStreams, Bytes>;
type SenderFuture = Pin<Box<dyn Future<Output = Result<Sender, Arc<Error>>> + Send>>;
type CachedSender = Shared<SenderFuture>;

macro_rules! http_method {
    ($name:ident, $variant:expr) => {
        #[inline]
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
    /// Creates a new [`IrohH3Client`] using the given [`iroh::Endpoint`] and ALPN string.
    pub fn new(endpoint: Endpoint, alpn: Vec<u8>) -> Self {
        let inner = ClientInner {
            endpoint,
            alpn,
            sender_cache: Default::default(),
        }
        .into();
        Self { inner }
    }

    /// Extracts the [`EndpointId`] from the authority component of a URI.
    ///
    /// # Errors
    /// Returns [`Error::MissingAuthority`] if the URI has no authority, or
    /// [`Error::BadPeerId`] if the authority cannot be parsed as an [`EndpointId`].
    fn peer_id(uri: &Uri) -> Result<EndpointId, Error> {
        let authority = uri.authority().ok_or(Error::MissingAuthority)?.as_str();

        authority.parse().map_err(Error::BadPeerId)
    }

    /// Creates a request builder bound to this client
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

    /// Retrieves or establishes an HTTP/3 sender (`SendRequest`) for the specified peer.
    ///
    /// This method first attempts an **optimistic cache lookup** using a read-only
    /// lock. If an active or pending connection already exists, it reuses the
    /// existing shared future. Otherwise, it proceeds to coordinate the creation
    /// of a new connection using a write lock.
    ///
    /// The method ensures that concurrent calls for the same peer share the same
    /// in-progress connection setup, avoiding redundant HTTP/3 handshakes.
    ///
    /// Once the connection becomes idle, it is automatically removed from the cache.
    ///
    /// # Concurrency
    ///
    /// - Multiple simultaneous calls for the same `peer_id` await the same shared
    ///   future, ensuring only a single connection attempt is made.
    /// - The cache is protected by a read–write lock to support safe concurrent access.
    async fn get_sender(
        &self,
        peer_id: EndpointId,
    ) -> Result<SendRequest<OpenStreams, Bytes>, Error> {
        if let Some(sender) = self.try_get_cached_sender(peer_id).await {
            return Ok(sender);
        }
        self.coordinate_connection_setup(peer_id).await
    }

    /// Attempts an optimistic cache lookup for an existing sender using a read-only lock.
    ///
    /// This method acquires a **read** guard on the sender cache to check whether a
    /// shared future for the given `peer_id` already exists. If found, it awaits the
    /// cached future and returns the resulting `SendRequest` if the future resolves
    /// successfully.
    ///
    /// If no entry exists, or the cached future fails, the caller is expected to
    /// proceed with establishing a new connection.
    ///
    /// # Returns
    ///
    /// - `Some(sender)` if a valid cached sender is found.
    /// - `None` if no cached sender exists or it failed.
    async fn try_get_cached_sender(
        &self,
        peer_id: EndpointId,
    ) -> Option<SendRequest<OpenStreams, Bytes>> {
        let cached_sender = self
            .inner
            .sender_cache
            .read()
            .unwrap()
            .get(&peer_id)
            .cloned();

        if let Some(sender_future) = cached_sender {
            match sender_future.await {
                Ok(sender) => return Some(sender),
                Err(_) => {
                    // Ignore and let caller handle reconnect logic
                }
            }
        }
        None
    }

    /// Coordinates connection setup for a given peer by acquiring a write lock on the cache.
    ///
    /// This function handles synchronization between concurrent tasks that may attempt
    /// to connect to the same peer simultaneously. It operates in a loop to ensure that:
    ///
    /// - If another task has already inserted a shared connection future into the cache,
    ///   this call will reuse it and await the result.
    /// - If no existing connection future is present, a new one is created via
    ///   [`create_connection`] and inserted into the cache.
    /// - If a connection attempt fails, the cache entry is removed and the loop retries.
    ///
    /// # Returns
    ///
    /// - `Ok(sender)` on successful connection.
    /// - `Err(error)` if all attempts fail.
    async fn coordinate_connection_setup(
        &self,
        peer_id: EndpointId,
    ) -> Result<SendRequest<OpenStreams, Bytes>, Error> {
        loop {
            enum Action {
                TryForeign(Shared<SenderFuture>),
                CreateOwn(Shared<SenderFuture>),
            }

            let action = {
                let mut cache = self.inner.sender_cache.write().unwrap();
                if let Some(sender_future) = cache.get(&peer_id).cloned() {
                    Action::TryForeign(sender_future)
                } else {
                    let sender_future = self.create_connection(peer_id);
                    cache.insert(peer_id, sender_future.clone());
                    Action::CreateOwn(sender_future)
                }
            };

            match action {
                Action::TryForeign(shared) => {
                    if let Ok(sender) = shared.await {
                        return Ok(sender);
                    }
                }
                Action::CreateOwn(shared) => {
                    return match shared.await {
                        Ok(sender) => Ok(sender),
                        Err(error) => {
                            self.inner.sender_cache.write().unwrap().remove(&peer_id);
                            Err(error.into())
                        }
                    };
                }
            }
        }
    }

    /// Initiates a new HTTP/3 connection to the specified peer and returns a
    /// shared future resolving to a [`SendRequest<OpenStreams, Bytes>`].
    ///
    /// This function encapsulates the connection setup process for a given
    /// `peer_id`. It performs the following steps:
    ///
    /// 1. Establishes a new QUIC connection to the target endpoint using the
    ///    configured [`Endpoint`] and ALPN protocol.
    /// 2. Wraps the QUIC connection in an [`IrohH3Connection`] and performs the
    ///    HTTP/3 handshake via `h3::client::new`.
    /// 3. Spawns a background task that waits for the connection to become idle
    ///    (`conn.wait_idle()`) and removes the corresponding sender from the
    ///    internal cache once it is no longer active.
    ///
    /// The returned future is converted into a [`Shared`] future so that
    /// concurrent callers awaiting the same connection setup will all share
    /// the same result, avoiding redundant connection attempts.
    ///
    /// # Returns
    ///
    /// A [`Shared`] future that resolves to either:
    /// - `Ok(SendRequest<OpenStreams, Bytes>)` on successful connection and
    ///   HTTP/3 handshake.
    /// - `Err(Arc<Error>)` if any step of the setup fails.
    fn create_connection(
        &self,
        peer_id: EndpointId,
    ) -> Shared<
        Pin<Box<dyn Future<Output = Result<SendRequest<OpenStreams, Bytes>, Arc<Error>>> + Send>>,
    > {
        let self_clone = self.clone();
        let future = Box::pin(async move {
            let conn = self_clone
                .inner
                .endpoint
                .connect(peer_id, &self_clone.inner.alpn)
                .await
                .map_err(Error::from)
                .map_err(Arc::new)?;
            let conn = IrohH3Connection::new(conn);
            let (mut conn, sender) = h3::client::new(conn)
                .await
                .map_err(Error::from)
                .map_err(Arc::new)?;
            tokio::spawn(async move {
                conn.wait_idle().await;
                self_clone
                    .inner
                    .sender_cache
                    .write()
                    .unwrap()
                    .remove(&peer_id);
            });
            Ok(sender)
        }) as SenderFuture;
        let shared = future.shared();
        shared
    }

    /// Sends an HTTP/3 request to the peer identified in the request URI.
    ///
    /// This method automatically:
    /// - Ensures the request version is set to [`Version::HTTP_3`]
    /// - Resolves the peer from the URI authority
    /// - Establishes a QUIC + HTTP/3 connection
    /// - Sends the full request and returns a [`Response`] handle
    ///
    /// # Errors
    /// Returns an [`Error`] if connection setup, sending, or response reception fails.
    pub async fn send<B>(
        &self,
        request: impl TryInto<http::Request<B>, Error = impl Into<http::Error>>,
    ) -> Result<Response, Error>
    where
        B: Body,
        http::Error: From<B::Error>,
    {
        let mut request = request.try_into().map_err(Into::<http::Error>::into)?;

        *request.version_mut() = Version::HTTP_3;

        let peer_id = Self::peer_id(request.uri())?;
        let mut sender = self.get_sender(peer_id).await?;

        let (parts, body) = request.into_parts();
        let req = http::Request::from_parts(parts, ());

        let mut stream = sender.send_request(req).await?;

        Self::send_body(&mut stream, body).await?;

        stream.finish().await?;

        // Receive the initial response headers.
        let response = stream.recv_response().await?;
        let inner = response.into_parts().0;

        Ok(Response {
            inner,
            stream,
            _sender: sender,
        })
    }

    /// Internal function for sending a request body
    async fn send_body<B>(
        stream: &mut RequestStream<BidiStream<Bytes>, Bytes>,
        body: B,
    ) -> Result<(), Error>
    where
        B: Body,
        http::Error: From<B::Error>,
    {
        let mut body_stream = BodyStream::new(Box::pin(body));
        loop {
            match body_stream
                .next()
                .await
                .transpose()
                .map_err(Into::<http::Error>::into)?
            {
                Some(frame) if frame.is_data() => {
                    let mut data = frame
                        .into_data()
                        .ok()
                        .expect("Non-data frame in a branch guarded by is_data");
                    let buf = data.copy_to_bytes(data.remaining());
                    stream.send_data(buf).await?;
                }
                Some(frame) if frame.is_trailers() => {
                    let trailers = frame
                        .into_trailers()
                        .ok()
                        .expect("Non-trailers frame in a branch guarded by is_trailers");
                    stream.send_trailers(trailers).await?;
                }
                Some(_) => unimplemented!("Unexpected frame type"),
                None => break,
            }
        }
        Ok(())
    }
}
