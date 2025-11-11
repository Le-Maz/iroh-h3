use std::{convert::Infallible, sync::Arc};

use h3::error::{ConnectionError, StreamError};
use iroh::{KeyParsingError, endpoint::ConnectError};

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

    #[error("{0}")]
    Shared(#[from] Arc<Self>),
}

impl From<Infallible> for Error {
    fn from(_: Infallible) -> Self {
        unreachable!()
    }
}
