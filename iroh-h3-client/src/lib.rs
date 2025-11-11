pub mod error;
pub mod request;
pub mod response;

use std::future::Future;

use bytes::{Buf, Bytes};
use futures::StreamExt;
use h3::client::RequestStream;
use http::request::Builder;
use http::{Method, Uri, Version};
use http_body::Body;
use http_body_util::BodyStream;
use iroh::{Endpoint, EndpointId};
use iroh_h3::{BidiStream, Connection as IrohH3Connection};

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
/// let endpoint = iroh::Endpoint::builder().bind(0).await?;
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
#[derive(Debug)]
pub struct IrohH3Client {
    endpoint: Endpoint,
    alpn: Vec<u8>,
}

macro_rules! http_method {
    ($name:ident, $variant:expr) => {
        #[inline]
        pub fn $name<'client, U>(&'client self, uri: U) -> RequestBuilder<'client>
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
        Self { endpoint, alpn }
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
    pub fn request<'client, U>(
        &'client self,
        method: http::Method,
        uri: U,
    ) -> RequestBuilder<'client>
    where
        U: TryInto<Uri>,
        http::Error: From<<U as TryInto<Uri>>::Error>,
    {
        RequestBuilder {
            inner: Builder::new().method(method).uri(uri),
            client: self,
        }
    }

    http_method!(head, Method::HEAD);
    http_method!(get, Method::GET);
    http_method!(post, Method::POST);
    http_method!(put, Method::PUT);
    http_method!(patch, Method::PATCH);
    http_method!(delete, Method::DELETE);

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
        let conn = self.endpoint.connect(peer_id, &self.alpn).await?;

        let conn = IrohH3Connection::new(conn);
        let (mut conn, mut sender) = h3::client::new(conn).await?;

        let (parts, body) = request.into_parts();
        let req = http::Request::from_parts(parts, ());

        let mut stream = conn.process(sender.send_request(req)).await?;

        conn.process(Self::send_body(&mut stream, body)).await?;

        stream.finish().await?;

        // Receive the initial response headers.
        let response = conn.process(stream.recv_response()).await?;
        let inner = response.into_parts().0;

        Ok(Response {
            inner,
            stream,
            conn,
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

/// Helper trait to wrap connection operations with cancellation on idle.
///
/// This allows the connection to await an operation (like sending a request or
/// reading a response) while simultaneously listening for connection's status changes.
pub(crate) trait ConnectionProcess {
    /// Runs a future tied to a connection and converts errors into [`Error`].
    fn process<T, E>(
        &mut self,
        future: impl Future<Output = Result<T, E>>,
    ) -> impl Future<Output = Result<T, Error>>
    where
        Error: From<E>;
}

impl ConnectionProcess for h3::client::Connection<IrohH3Connection, Bytes> {
    async fn process<T, E>(
        &mut self,
        future: impl Future<Output = Result<T, E>>,
    ) -> Result<T, Error>
    where
        Error: From<E>,
    {
        tokio::select! {
            result = future => Ok(result?),
            result = self.wait_idle() => Err(result.into()),
        }
    }
}
