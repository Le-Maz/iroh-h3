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

use std::ops::Deref;
use std::pin::Pin;
use std::task::Poll;

use bytes::{Buf, Bytes};
use futures::Stream;
use iroh_h3::{BidiStream, OpenStreams};
#[cfg(feature = "json")]
use serde::de::DeserializeOwned;

use crate::error::Error;

/// Represents an HTTP/3 response received from an [`IrohH3Client`].
///
/// This type provides access to the response’s headers and body, which can be
/// consumed all at once via [`bytes`](Self::bytes), deserialized from JSON via
/// [`json`](Self::json), or streamed incrementally using [`bytes_stream`](Self::bytes_stream).
#[must_use]
pub struct Response {
    pub(crate) inner: http::response::Parts,
    pub(crate) stream: h3::client::RequestStream<BidiStream<Bytes>, Bytes>,
    pub(crate) _sender: h3::client::SendRequest<OpenStreams, Bytes>,
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
    pub async fn bytes(&mut self) -> Result<Bytes, Error> {
        let mut buf = Vec::new();

        loop {
            match self.stream.recv_data().await {
                Ok(Some(mut frame)) => {
                    while frame.has_remaining() {
                        let chunk = frame.chunk();
                        buf.extend_from_slice(chunk);
                        frame.advance(chunk.len());
                    }
                }
                Ok(None) => break,
                Err(err) if err.is_h3_no_error() => break,
                Err(err) => return Err(err.into()),
            }
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
    pub async fn text(&mut self) -> Result<String, Error> {
        let bytes = self.bytes().await?;
        let string = String::from_utf8(bytes.to_vec())
            .map_err(|err| Error::InvalidUtf8(err.utf8_error()))?;
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
    pub async fn json<T: DeserializeOwned>(&mut self) -> Result<T, Error> {
        let bytes = self.bytes().await?;
        Ok(serde_json::from_slice(&bytes)?)
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
    pub fn bytes_stream(&mut self) -> impl Stream<Item = Result<Bytes, Error>> {
        struct StreamingBody<'response> {
            response: &'response mut Response,
        }

        impl<'response> Stream for StreamingBody<'response> {
            type Item = Result<Bytes, Error>;

            fn poll_next(
                mut self: Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                let poll_data = self.response.stream.poll_recv_data(cx);
                if let Poll::Ready(result) = poll_data {
                    let item = match result.transpose() {
                        Some(Ok(mut frame)) => Some(Ok(frame.copy_to_bytes(frame.remaining()))),
                        Some(Err(error)) if !error.is_h3_no_error() => Some(Err(error.into())),
                        _ => None,
                    };
                    return Poll::Ready(item);
                }
                Poll::Pending
            }
        }

        StreamingBody { response: self }
    }
}
