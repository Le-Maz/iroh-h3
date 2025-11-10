use std::ops::Deref;

use bytes::{Buf, Bytes};
use h3::error::{ConnectionError, StreamError};
use http::{Request, Uri, Version};
use iroh::{EndpointId, KeyParsingError, endpoint::ConnectError};
use iroh_h3::BidiStream;

pub struct IrohH3Client {
    endpoint: iroh::Endpoint,
    alpn: Vec<u8>,
}

impl IrohH3Client {
    fn peer_id(uri: &Uri) -> Result<EndpointId, Error> {
        let authority = uri.authority().ok_or(Error::MissingAuthority)?.as_str();
        authority.parse().map_err(Error::BadPeerId)
    }

    pub async fn send<B: Buf>(&self, mut request: Request<B>) -> Result<Response, Error> {
        *request.version_mut() = Version::HTTP_3;
        let peer_id = Self::peer_id(request.uri())?;
        let conn = self.endpoint.connect(peer_id, &self.alpn).await?;
        let conn = iroh_h3::Connection::new(conn);
        let (mut conn, mut send_request) = h3::client::new(conn).await?;
        let (parts, mut body) = request.into_parts();
        let req = http::Request::from_parts(parts, ());
        let mut stream = conn.process(send_request.send_request(req)).await?;
        let buf = body.copy_to_bytes(body.remaining());
        conn.process(stream.send_data(buf)).await?;
        let response = conn.process(stream.recv_response()).await?;
        let inner = response.into_parts().0;
        Ok(Response {
            inner,
            stream,
            conn,
        })
    }
}

pub struct Response {
    inner: http::response::Parts,
    stream: h3::client::RequestStream<BidiStream<Bytes>, Bytes>,
    conn: h3::client::Connection<iroh_h3::Connection, Bytes>,
}

impl Deref for Response {
    type Target = http::response::Parts;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Response {
    pub async fn body_bytes(&mut self) -> Result<Bytes, Error> {
        let mut buf = Vec::new();
        while let Some(mut response_body) = self.conn.process(self.stream.recv_data()).await? {
            while response_body.has_remaining() {
                let chunk = response_body.chunk();
                buf.extend_from_slice(chunk);
                response_body.advance(chunk.len());
            }
        }
        Ok(Bytes::copy_from_slice(&buf))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Missing URI authority")]
    MissingAuthority,
    #[error("Bad peer ID: {0}")]
    BadPeerId(KeyParsingError),
    #[error("Other HTTP error: {0}")]
    Http(#[from] http::Error),
    #[error("{0}")]
    Connect(#[from] ConnectError),
    #[error("{0}")]
    Connection(#[from] ConnectionError),
    #[error("{0}")]
    Stream(#[from] StreamError),
}

trait ConnectionProcess {
    fn process<T, E>(
        &mut self,
        future: impl Future<Output = Result<T, E>>,
    ) -> impl Future<Output = Result<T, Error>>
    where
        Error: From<E>;
}

impl ConnectionProcess for h3::client::Connection<iroh_h3::Connection, Bytes> {
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
