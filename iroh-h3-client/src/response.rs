//! HTTP/3 response handling.
//!
//! This module defines the [`Response`] type, which provides access to HTTP/3 response headers
//! and bodies.  
//!
//! Features include:
//! - Reading the full response body as [`Bytes`] or [`String`]
//! - Streaming response bodies incrementally for large payloads
//! - JSON deserialization when the `json` feature is enabled
//! - UTF-8 validation for text responses

pub mod sse;

use std::ops::Deref;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::{Buf, Bytes};
use futures::{Stream, StreamExt};
use http_body::Frame;
use http_body_util::BodyExt;
use iroh_h3::OpenStreams;
#[cfg(feature = "json")]
use serde::de::DeserializeOwned;
use tracing::{debug, instrument, trace};

use crate::body::Body;
use crate::error::Error;
use crate::response::sse::{SseEvent, SseStream};

/// Represents an HTTP/3 response received from an [`IrohH3Client`](crate::IrohH3Client).
///
/// This type provides access to the response’s headers and body, which can be
/// consumed all at once via [`bytes`](Self::bytes), or streamed incrementally
/// using [`bytes_stream`](Self::bytes_stream).
#[must_use]
pub struct Response {
    pub(crate) inner: http::response::Parts,
    pub(crate) body: Body,
}

impl Deref for Response {
    type Target = http::response::Parts;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Response {
    /// Reads the full response body into a contiguous [`Bytes`] buffer.
    ///
    /// This method consumes all HTTP/3 DATA frames from the response stream until
    /// the end of the stream or a graceful connection close.
    ///
    /// # Returns
    /// A [`Bytes`] object containing the entire response body.
    ///
    /// # Errors
    /// Returns an [`Error`] if a connection or stream error occurs during reading.
    ///
    /// # Example
    /// ```rust,ignore
    /// let mut response = client.send(request).await?;
    /// let body = response.bytes().await?;
    /// println!("Response body: {:?}", body);
    /// ```
    #[instrument(skip(self))]
    pub async fn bytes(self) -> Result<Bytes, Error> {
        let mut buf = Vec::new();
        let mut stream = self.bytes_stream();

        while let Some(data) = stream.next().await.transpose()? {
            debug!("received {} bytes", data.len());
            buf.extend_from_slice(&data);
        }

        Ok(Bytes::from(buf))
    }

    /// Reads the full response body and returns it as a UTF-8 [`String`].
    ///
    /// This method consumes all HTTP/3 DATA frames from the response stream and
    /// attempts to interpret the resulting bytes as UTF-8 text.
    ///
    /// # Returns
    /// A [`String`] containing the entire response body.
    ///
    /// # Errors
    /// Returns an [`Error`] if:
    /// - Reading the response body fails.
    /// - The response body contains invalid UTF-8 data.
    ///
    /// # Example
    /// ```rust,ignore
    /// let mut response = client.send(request).await?;
    /// let text = response.text().await?;
    /// println!("Response: {}", text);
    /// ```
    #[instrument(skip(self))]
    pub async fn text(self) -> Result<String, Error> {
        let bytes = self.bytes().await?;
        let string = String::from_utf8(bytes.to_vec()).map_err(|err| {
            debug!("UTF-8 conversion failed");
            Error::InvalidUtf8(err.utf8_error())
        })?;
        Ok(string)
    }

    /// Reads the response body in full and attempts to deserialize it as JSON.
    ///
    /// This method validates that the `Content-Type` header is set to
    /// `application/json`, then reads and parses the body into the specified type.
    ///
    /// # Type Parameters
    /// - `T`: The type to deserialize the JSON into. Must implement [`DeserializeOwned`].
    ///
    /// # Returns
    /// A value of type `T` deserialized from the response body.
    ///
    /// # Errors
    /// Returns an [`Error`] if:
    /// - The `Content-Type` header is missing or not `application/json`.
    /// - The response body cannot be read.
    /// - The response body cannot be parsed as valid JSON for the target type.
    ///
    /// # Example
    /// ```rust,ignore
    /// #[derive(serde::Deserialize)]
    /// struct ApiResponse { message: String }
    ///
    /// let mut response = client.send(request).await?;
    /// let data: ApiResponse = response.json().await?;
    /// println!("Message: {}", data.message);
    /// ```
    #[cfg(feature = "json")]
    #[instrument(skip(self))]
    pub async fn json<T: DeserializeOwned>(self) -> Result<T, Error> {
        let bytes = self.bytes().await?;
        let value = serde_json::from_slice(&bytes)?;
        debug!("parsed JSON successfully");
        Ok(value)
    }

    /// Returns an asynchronous stream of response body chunks.
    ///
    /// This method yields [`Bytes`] chunks as HTTP/3 DATA frames are received from
    /// the server, allowing you to process large or streaming responses without
    /// buffering the entire body in memory.
    ///
    /// # Returns
    /// A [`Stream`] that yields [`Result<Bytes, Error>`] values.
    ///
    /// # Errors
    /// Each stream item may return an [`Error`] if reading from the connection fails.
    ///
    /// # Example
    /// ```rust,ignore
    /// use futures::StreamExt;
    ///
    /// let mut response = client.send(request).await?;
    /// let mut stream = response.bytes_stream();
    ///
    /// while let Some(chunk) = stream.next().await.transpose()? {
    ///     println!("Received chunk: {:?}", chunk);
    /// }
    /// ```
    #[instrument(skip(self))]
    pub fn bytes_stream(self) -> impl Stream<Item = Result<Bytes, Error>> {
        self.body.into_stream().into_data_stream()
    }

    /// Returns a stream of Server-Sent Events
    #[instrument(skip(self))]
    pub fn sse_stream(self) -> impl Stream<Item = Result<SseEvent, Error>> {
        SseStream::new(self)
    }
}

/// HTTP/3 body implementing `http_body::Body`.
///
/// Wraps the `RequestStream` returned by iroh-h3.
pub(crate) struct IrohH3ResponseBody {
    pub(crate) stream: h3::client::RequestStream<iroh_h3::BidiStream<Bytes>, Bytes>,
    pub(crate) _sender: h3::client::SendRequest<OpenStreams, Bytes>,
}

impl IrohH3ResponseBody {
    pub(crate) fn new(
        stream: h3::client::RequestStream<iroh_h3::BidiStream<Bytes>, Bytes>,
        sender: h3::client::SendRequest<OpenStreams, Bytes>,
    ) -> Self {
        Self {
            stream,
            _sender: sender,
        }
    }
}

impl http_body::Body for IrohH3ResponseBody {
    type Data = Bytes;
    type Error = Error;

    #[instrument(skip(self, cx))]
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        match ready!(self.stream.poll_recv_data(cx)).transpose() {
            Some(Ok(mut frame)) => {
                trace!("received a frame of {} bytes", frame.remaining());
                let bytes = frame.copy_to_bytes(frame.remaining());
                Poll::Ready(Some(Ok(Frame::data(bytes))))
            }
            Some(Err(e)) => {
                if e.is_h3_no_error() {
                    debug!("received H3_NO_ERROR");
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Err(e.into())))
                }
            }
            None => Poll::Ready(None),
        }
    }
}
