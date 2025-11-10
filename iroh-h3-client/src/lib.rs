use std::future::Future;
use std::ops::Deref;

use bytes::{Buf, Bytes};
use h3::error::{ConnectionError, StreamError};
use http::{Request, Uri, Version};
use iroh::{Endpoint, EndpointId, KeyParsingError, endpoint::ConnectError};
use iroh_h3::{BidiStream, Connection as IrohH3Connection};

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
///     .uri("iroh://peer-id/some/path")
///     .body(Bytes::from("hello world"))?;
///
/// let mut response = client.send(request).await?;
/// let body = response.body_bytes().await?;
/// println!("Response body: {:?}", body);
/// ```
pub struct IrohH3Client {
    endpoint: Endpoint,
    alpn: Vec<u8>,
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
    pub async fn send<B: Buf>(&self, mut request: Request<B>) -> Result<Response, Error> {
        *request.version_mut() = Version::HTTP_3;

        let peer_id = Self::peer_id(request.uri())?;
        let conn = self.endpoint.connect(peer_id, &self.alpn).await?;

        let conn = IrohH3Connection::new(conn);
        let (mut conn, mut sender) = h3::client::new(conn).await?;

        let (parts, mut body) = request.into_parts();
        let req = Request::from_parts(parts, ());

        let mut stream = conn.process(sender.send_request(req)).await?;

        // Send the full body as one chunk.
        let buf = body.copy_to_bytes(body.remaining());
        conn.process(stream.send_data(buf)).await?;

        // Receive the initial response headers.
        let response = conn.process(stream.recv_response()).await?;
        let inner = response.into_parts().0;

        Ok(Response {
            inner,
            stream,
            conn,
        })
    }
}

/// Represents an HTTP/3 response received over an [`iroh`] connection.
///
/// Provides access to response headers and body data.
#[must_use]
pub struct Response {
    inner: http::response::Parts,
    stream: h3::client::RequestStream<BidiStream<Bytes>, Bytes>,
    conn: h3::client::Connection<IrohH3Connection, Bytes>,
}

impl Deref for Response {
    type Target = http::response::Parts;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Response {
    /// Reads the entire response body into a [`Bytes`] buffer.
    ///
    /// This method consumes all available HTTP/3 DATA frames until the stream ends
    /// or a graceful connection close occurs.
    ///
    /// # Errors
    /// Returns an [`Error`] if a connection or stream error occurs while reading.
    pub async fn body_bytes(&mut self) -> Result<Bytes, Error> {
        let mut buf = Vec::new();

        loop {
            match self.conn.process(self.stream.recv_data()).await {
                Ok(Some(mut frame)) => {
                    while frame.has_remaining() {
                        let chunk = frame.chunk();
                        buf.extend_from_slice(chunk);
                        frame.advance(chunk.len());
                    }
                }
                Ok(None) => break,
                Err(Error::Connection(err)) if err.is_h3_no_error() => break,
                Err(e) => return Err(e),
            }
        }

        Ok(Bytes::from(buf))
    }
}

/// Errors that can occur while sending or receiving HTTP/3 requests with [`IrohH3Client`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The URI did not contain an authority (peer ID).
    #[error("Missing URI authority")]
    MissingAuthority,

    /// Failed to parse the URI authority as a valid peer ID.
    #[error("Bad peer ID: {0}")]
    BadPeerId(KeyParsingError),

    /// General HTTP error (invalid request or response building).
    #[error("HTTP error: {0}")]
    Http(#[from] http::Error),

    /// Failed to establish a connection to the peer.
    #[error("Connection failed: {0}")]
    Connect(#[from] ConnectError),

    /// QUIC or HTTP/3 connection-level error.
    #[error("Connection error: {0}")]
    Connection(#[from] ConnectionError),

    /// HTTP/3 stream-level error.
    #[error("Stream error: {0}")]
    Stream(#[from] StreamError),
}

/// Helper trait to wrap connection operations with cancellation on idle.
///
/// This allows the connection to await an operation (like sending a request or
/// reading a response) while simultaneously listening for connection's status changes.
trait ConnectionProcess {
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
