use std::ops::Deref;
use std::pin::Pin;
use std::task::Poll;

use bytes::{Buf, Bytes};
use futures::Stream;
use iroh_h3::{BidiStream, OpenStreams};
#[cfg(feature = "json")]
use serde::de::DeserializeOwned;

use crate::error::Error;

/// Represents an HTTP/3 response received over an [`iroh`] connection.
///
/// Provides access to response headers and body data.
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
    /// Reads the entire response body into a [`Bytes`] buffer.
    ///
    /// This method consumes all available HTTP/3 DATA frames until the stream ends
    /// or a graceful connection close occurs.
    ///
    /// # Errors
    /// Returns an [`Error`] if a connection or stream error occurs while reading.
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

    /// Reads the entire response body and deserializes it from JSON.
    ///
    /// This method first verifies that the response `Content-Type` header is
    /// `application/json`, then reads the full response body into memory and
    /// attempts to deserialize it into the specified type.
    ///
    /// # Type Parameters
    /// - `T`: The type to deserialize the JSON body into. Must implement [`DeserializeOwned`].
    ///
    /// # Errors
    /// Returns an [`Error`] if:
    /// - The `Content-Type` header is missing or not `application/json`.
    /// - The body cannot be read from the stream.
    /// - The body cannot be parsed as valid JSON for type `T`.
    #[cfg(feature = "json")]
    pub async fn json<T: DeserializeOwned>(&mut self) -> Result<T, Error> {
        use http::{HeaderValue, header::CONTENT_TYPE};

        use crate::error::JsonError;
        const MIME_JSON: HeaderValue = HeaderValue::from_static("application/json");

        let content_type = self.headers.get(CONTENT_TYPE);
        if content_type != Some(&MIME_JSON) {
            use crate::error::JsonError;

            return Err(JsonError::WrongContentType(content_type.cloned()).into());
        }

        let bytes = self.bytes().await?;
        Ok(serde_json::from_slice(&bytes).map_err(JsonError::from)?)
    }

    /// Returns a stream of [`Bytes`] representing the response body.
    ///
    /// This method provides an asynchronous stream of chunks of the response body.
    /// It consumes HTTP/3 DATA frames as they arrive, allowing the caller to process
    /// the body incrementally without waiting for the entire response to be received.
    ///
    /// # Errors
    /// Returns an [`Error`] if a connection or stream error occurs while reading.
    ///
    /// # Example
    /// ```rust,ignore
    /// let mut response = client.send(request).await?;
    /// let mut body_stream = response.stream();
    /// while let Some(data) = body_stream.next().await.transpose()? {
    ///     println!("Received frame: {:?}", data);
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
