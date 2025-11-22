//! Error types for the `iroh-h3` library.
//!
//! This module defines all errors that can occur when using the HTTP/3 client,
//! organized to make handling easy and ergonomic.
//!
//! The primary error type is [`Error`], which wraps all lower-level errors into
//! broad categories:
//!
//! - [`RequestValidationError`] — Errors that occur while preparing or validating
//!   a request, such as:
//!     - Missing or invalid URI authority (peer ID)
//!     - JSON serialization errors (if the `json` feature is enabled)
//!
//! - [`ResponseValidationError`] — Errors that occur when handling a response, such as:
//!     - Invalid UTF-8 in response bodies
//!     - JSON deserialization errors (if the `json` feature is enabled)
//!
//! - [`TransportError`] — Errors related to the network transport layer, including:
//!     - Connection failures
//!     - QUIC or HTTP/3 session-level errors
//!     - Stream-level errors
//!
//! - [`http::Error`] — General HTTP protocol errors.
//!
//! - `Other` — Miscellaneous errors that don’t fit into any specific category.
//!
//! The error types are designed to be **extensible** and **composable**, supporting
//! common patterns like wrapping in an `Arc` for sharing across async tasks.

use std::{convert::Infallible, sync::Arc};

use h3::error::{ConnectionError, StreamError};
use iroh::{KeyParsingError, endpoint::ConnectError};

/// Main error type for the Iroh HTTP/3 client.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Errors encountered while building or sending requests.
    #[error("Request validation error: {0}")]
    RequestValidation(#[from] RequestValidationError),

    /// Errors encountered while receiving or parsing responses.
    #[error("Response validation error: {0}")]
    ResponseValidation(#[from] ResponseValidationError),

    /// Transport-level errors (connections, QUIC, streams).
    #[error("Transport error: {0}")]
    Transport(#[from] TransportError),

    /// HTTP protocol errors (invalid request/response structure).
    #[error("HTTP error: {0}")]
    Http(#[from] http::Error),

    /// Shared error wrapped in an Arc for safe cloning.
    #[error("{0}")]
    Shared(#[from] Arc<Self>),

    /// Miscellaneous or general-purpose error with a human-readable message.
    #[error("{0}")]
    Other(String),
}

/// Errors encountered while preparing or validating requests.
#[derive(Debug, thiserror::Error)]
pub enum RequestValidationError {
    /// Missing authority (peer ID) in URI.
    #[error("Missing URI authority")]
    MissingAuthority,

    /// Bad peer ID in URI.
    #[error("Bad peer ID: {0}")]
    BadPeerId(KeyParsingError),

    /// JSON serialization error (requires `json` feature).
    #[cfg(feature = "json")]
    #[error("JSON serialization error: {0}")]
    JsonSerialize(#[from] serde_json::Error),
}

/// Errors encountered while validating or parsing responses.
#[derive(Debug, thiserror::Error)]
pub enum ResponseValidationError {
    /// Invalid UTF-8 in response body.
    #[error("Invalid UTF-8: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),

    /// JSON deserialization error (requires `json` feature).
    #[cfg(feature = "json")]
    #[error("JSON deserialization error: {0}")]
    JsonDeserialize(#[from] serde_json::Error),
}

/// Transport-level errors (connection or stream issues).
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Failed to connect to the remote peer.
    #[error("Connect failed: {0}")]
    Connect(#[from] ConnectError),

    /// QUIC or HTTP/3 session-level connection error.
    #[error("QUIC/HTTP3 connection error: {0}")]
    Connection(#[from] ConnectionError),

    /// Stream-level error (e.g., unexpected frame sequence).
    #[error("Stream error: {0}")]
    Stream(#[from] StreamError),
}

impl From<Infallible> for Error {
    fn from(_: Infallible) -> Self {
        unreachable!()
    }
}
