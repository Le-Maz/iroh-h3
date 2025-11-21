//! Error types for the `iroh-h3` library.
//!
//! This module defines all errors that can occur while using the HTTP/3 client, including:
//! - Connection errors
//! - Stream errors
//! - Serialization or deserialization errors (e.g., JSON)
//! - Invalid UTF-8 in text responses
//!
//! The primary error type is [`Error`], which wraps all lower-level errors in a single type.
//!
//! All methods that perform I/O or parsing return a `Result<_, Error>` to unify error handling.

use std::{convert::Infallible, sync::Arc};

use h3::error::{ConnectionError, StreamError};
use iroh::{KeyParsingError, endpoint::ConnectError};

/// Errors that may occur while sending or receiving HTTP/3 requests using [`IrohH3Client`](crate::IrohH3Client).
///
/// This type aggregates possible error sources, including HTTP/3 protocol errors,
/// connection failures, malformed URIs, and optional JSON serialization issues.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The provided URI is missing an authority component (peer ID).
    ///
    /// This usually occurs when a URI lacks a valid peer identifier required
    /// for routing the request in the Iroh network.
    #[error("Missing URI authority")]
    MissingAuthority,

    /// Failed to parse the URI authority as a valid Iroh peer ID.
    ///
    /// Wraps a [`KeyParsingError`] returned when the authority portion of the URI
    /// does not correspond to a valid peer key.
    #[error("Bad peer ID: {0}")]
    BadPeerId(KeyParsingError),

    /// General HTTP error.
    ///
    /// This may indicate an invalid request or response structure, such as malformed
    /// headers or an invalid body type.
    #[error("HTTP error: {0}")]
    Http(#[from] http::Error),

    /// Failed to establish a connection to the remote peer.
    ///
    /// Wraps a [`ConnectError`] returned from the Iroh endpoint.
    #[error("Connection failed: {0}")]
    Connect(#[from] ConnectError),

    /// A connection-level QUIC or HTTP/3 error occurred.
    ///
    /// Indicates that the underlying transport or session failed while communicating
    /// with the peer. Wraps a [`ConnectionError`].
    #[error("Connection error: {0}")]
    Connection(#[from] ConnectionError),

    /// An HTTP/3 stream-level error occurred.
    ///
    /// This variant represents errors related to individual bidirectional streams,
    /// such as unexpected frame sequences or stream termination errors.
    #[error("Stream error: {0}")]
    Stream(#[from] StreamError),

    /// A JSON serialization or deserialization error.
    ///
    /// Available only when the `json` feature is enabled.
    #[cfg(feature = "json")]
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// A shared error wrapped in an [`Arc`].
    ///
    /// This allows error instances to be safely cloned and shared across async tasks.
    #[error("{0}")]
    Shared(#[from] Arc<Self>),

    /// The response body contained invalid UTF-8 data.
    ///
    /// This error occurs when attempting to interpret a byte sequence
    /// as UTF-8 text using [`Response::text`](crate::response::Response::text),
    /// but the data is not valid UTF-8.
    #[error("Invalid UTF-8: {0}")]
    InvalidUtf8(std::str::Utf8Error),

    /// A general-purpose error with a human-readable message.
    ///
    /// This variant is used for errors that do not neatly fall into any of the
    /// more specific categories. It is primarily intended for middleware and
    /// utility layers that need to return an error without introducing new
    /// structured variants.
    #[error("{0}")]
    Other(String),
}

impl From<Infallible> for Error {
    fn from(_: Infallible) -> Self {
        unreachable!()
    }
}
