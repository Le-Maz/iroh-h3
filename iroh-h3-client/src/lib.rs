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

#![deny(missing_docs)]

pub mod error;
pub mod request;
pub mod response;

use std::fmt::Debug;
use std::pin::Pin;
use std::sync::Arc;

use bytes::{Buf, Bytes};
use dashmap::{DashMap, Entry};
use futures::future::Shared;
use futures::{FutureExt, StreamExt};
use h3::client::{RequestStream, SendRequest};
use http::request::Builder;
use http::{Method, Uri, Version};
use http_body::Body;
use http_body_util::BodyStream;
use iroh::{Endpoint, EndpointId};
use iroh_h3::{BidiStream, Connection as IrohH3Connection, OpenStreams};
use tracing::warn;

use crate::error::Error;
use crate::request::RequestBuilder;
use crate::response::Response;

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
/// let mut response = client.send(request).await?;
/// let body = response.bytes().await?;
/// println!("Response: {:?}", body);
/// # Ok(())
/// # }
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
    sender_cache: DashMap<EndpointId, CachedSender>,
}

type Sender = SendRequest<OpenStreams, Bytes>;
type SenderFuture = Pin<Box<dyn Future<Output = Result<Sender, Arc<Error>>> + Send>>;
type CachedSender = Shared<SenderFuture>;

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
    ///
    /// Returns:
    /// - [`Error::MissingAuthority`] if the URI lacks an authority.
    /// - [`Error::BadPeerId`] if the authority is not a valid [`EndpointId`].
    fn peer_id(uri: &Uri) -> Result<EndpointId, Error> {
        let authority = uri.authority().ok_or(Error::MissingAuthority)?.as_str();
        authority.parse().map_err(Error::BadPeerId)
    }

    /// Creates a new [`RequestBuilder`] associated with this client.
    ///
    /// This is the generic entry point for constructing an HTTP/3 request with
    /// a custom method.
    ///
    /// # Example
    /// ```rust,ignore
    /// let req = client.request(http::Method::PUT, "iroh+h3://peer/some/path");
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

    /// Retrieves or establishes an HTTP/3 sender for a peer.
    ///
    /// This method transparently manages the connection cache, attempting to reuse
    /// existing connections before creating new ones. If multiple concurrent tasks
    /// request a sender for the same peer, only a single connection setup is
    /// performed; other tasks await its completion.
    ///
    /// Once the connection becomes idle or closes, it is removed from the cache.
    async fn get_sender(
        &self,
        peer_id: EndpointId,
    ) -> Result<SendRequest<OpenStreams, Bytes>, Error> {
        if let Some(sender) = self.try_get_cached_sender(peer_id).await {
            return Ok(sender);
        }
        self.coordinate_connection_setup(peer_id).await
    }

    /// Attempts to retrieve a cached HTTP/3 sender for a given peer.
    ///
    /// If an active or pending connection future is cached, it awaits that shared
    /// future and returns the resulting sender if successful. If no cached future
    /// exists, this returns `None`.
    async fn try_get_cached_sender(
        &self,
        peer_id: EndpointId,
    ) -> Option<SendRequest<OpenStreams, Bytes>> {
        let cached_sender = self.inner.sender_cache.get(&peer_id).as_deref().cloned();

        if let Some(sender_future) = cached_sender {
            match sender_future.await {
                Ok(sender) => return Some(sender),
                Err(_) => {
                    // Connection failed or expired; remove and allow reconnect.
                }
            }
        }
        None
    }

    /// Coordinates concurrent connection setup for the same peer.
    ///
    /// This method uses a `DashMap` entry API to ensure that only one
    /// connection attempt is active per [`EndpointId`]. All other concurrent calls
    /// share the same future via [`Shared`].
    ///
    /// On failure, the cache entry is cleaned up to allow retry.
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
                let entry = self.inner.sender_cache.entry(peer_id);
                if let Entry::Occupied(sender_future) = entry {
                    Action::TryForeign(sender_future.get().clone())
                } else {
                    let sender_future = self.create_connection(peer_id);
                    entry.insert(sender_future.clone());
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
                            self.inner.sender_cache.remove(&peer_id);
                            Err(error.into())
                        }
                    };
                }
            }
        }
    }

    /// Creates and manages a new HTTP/3 connection for the specified peer.
    ///
    /// This function handles:
    /// - QUIC connection establishment via [`Endpoint::connect`].
    /// - HTTP/3 handshake via [`h3::client::new`].
    /// - Automatic removal of the sender from cache once the connection is idle.
    ///
    /// The result is wrapped in a [`Shared`] future to prevent duplicate setup
    /// for concurrent requests.
    fn create_connection(&self, peer_id: EndpointId) -> Shared<SenderFuture> {
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

            // Background cleanup task: remove sender when connection closes.
            tokio::spawn(async move {
                let error = conn.wait_idle().await;
                if !error.is_h3_no_error() {
                    warn!("Connection closed with error: {error}");
                }
                self_clone.inner.sender_cache.remove(&peer_id);
            });

            Ok(sender)
        }) as SenderFuture;

        future.shared()
    }

    /// Sends an HTTP/3 request and awaits the response.
    ///
    /// This method performs all steps required to send a request:
    ///
    /// 1. Validates and extracts the peer ID from the request URI.
    /// 2. Establishes or reuses an HTTP/3 connection to that peer.
    /// 3. Sends the request headers and body.
    /// 4. Waits for and returns the response headers.
    ///
    /// # Errors
    /// Returns an [`Error`] if any stage of connection setup, request sending,
    /// or response reception fails.
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

        let response = stream.recv_response().await?;
        let inner = response.into_parts().0;

        Ok(Response {
            inner,
            stream,
            _sender: sender,
        })
    }

    /// Sends an HTTP body over the given request stream.
    ///
    /// Consumes all frames emitted by the provided [`Body`] and transmits them
    /// as HTTP/3 DATA or TRAILERS frames.
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
                Some(_) => warn!("Unexpected frame type"),
                None => break,
            }
        }
        Ok(())
    }
}
