//! HTTP/3 request building and sending.
//!
//! This module provides [`RequestBuilder`] and [`Request`] types for constructing HTTP/3 requests
//! and sending them via an [`IrohH3Client`].  
//!
//! Features include:
//! - Setting headers and extensions
//! - Sending plain text, binary, or JSON payloads (with the `json` feature)
//! - Automatic setting of appropriate `Content-Type` headers

use bytes::Bytes;
use http::request::Builder;
use http::{HeaderValue, header::CONTENT_TYPE};
use http_body::Body;
use http_body_util::{Empty, Full};
#[cfg(feature = "json")]
use serde::Serialize;

use crate::{IrohH3Client, error::Error, response::Response};

/// A builder for constructing HTTP/3 requests.
///
/// This struct provides methods to configure and send HTTP/3 requests using
/// the [`IrohH3Client`]. It allows setting headers, extensions, and the
/// request body in various formats.
#[derive(Debug)]
#[must_use]
pub struct RequestBuilder {
    pub(crate) inner: Builder,
    pub(crate) client: IrohH3Client,
}

impl RequestBuilder {
    /// Adds an extension to the request.
    #[inline]
    pub fn extension<T>(mut self, extension: T) -> Self
    where
        T: Clone + std::any::Any + Send + Sync + 'static,
    {
        self.inner = self.inner.extension(extension);
        self
    }

    /// Adds a header to the request.
    #[inline]
    pub fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        K: TryInto<http::HeaderName>,
        <K as TryInto<http::HeaderName>>::Error: Into<http::Error>,
        V: TryInto<http::HeaderValue>,
        <V as TryInto<http::HeaderValue>>::Error: Into<http::Error>,
    {
        self.inner = self.inner.header(key, value);
        self
    }

    /// Builds a request with the given body.
    #[inline]
    pub fn body<B>(self, body: B) -> Result<Request<B>, Error>
    where
        B: Body,
        http::Error: From<B::Error>,
    {
        let request = self.inner.body(body)?;
        Ok(Request {
            inner: request,
            client: self.client,
        })
    }

    /// Builds a request with an empty body.
    #[inline]
    pub fn build(self) -> Result<Request<Empty<Bytes>>, Error> {
        self.body(Empty::new())
    }

    /// Ensures that the request has a `Content-Type` header set.
    ///
    /// If a `Content-Type` is not already present, this method adds it
    /// using the provided value. Returns the modified builder.
    ///
    /// This helper is used by [`Self::text`], [`Self::bytes`], and
    /// [`Self::json`] to avoid overwriting manually specified headers.
    #[inline]
    fn ensure_content_type(mut self, value: HeaderValue) -> Self {
        if self
            .inner
            .headers_ref()
            .is_none_or(|headers| headers.get(CONTENT_TYPE).is_none())
        {
            self.inner = self.inner.header(CONTENT_TYPE, value);
        }
        self
    }

    /// Sets the request body to the given UTF-8 text.
    ///
    /// Automatically sets the `Content-Type` header to
    /// `"text/plain; charset=utf-8"` **if it is not already set**.
    ///
    /// # Errors
    /// Returns an [`Error`] if the request cannot be constructed.
    #[inline]
    pub fn text(self, text: impl AsRef<str>) -> Result<Request<Full<Bytes>>, Error> {
        const MIME_TEXT: HeaderValue = HeaderValue::from_static("text/plain; charset=utf-8");

        let body_bytes = Bytes::copy_from_slice(text.as_ref().as_bytes());
        self.ensure_content_type(MIME_TEXT)
            .body(Full::new(body_bytes))
    }

    /// Sets the request body to the given binary bytes.
    ///
    /// Automatically sets the `Content-Type` header to
    /// `"application/octet-stream"` **if it is not already set**.
    ///
    /// # Errors
    /// Returns an [`Error`] if the request cannot be constructed.
    #[inline]
    pub fn bytes(self, bytes: impl Into<Bytes>) -> Result<Request<Full<Bytes>>, Error> {
        const MIME_BIN: HeaderValue = HeaderValue::from_static("application/octet-stream");

        self.ensure_content_type(MIME_BIN)
            .body(Full::new(bytes.into()))
    }

    /// Sets the body of the request to JSON-serialized data.
    ///
    /// Automatically sets the `Content-Type` header to
    /// `"application/json"` **if it is not already set**.
    ///
    /// Requires the `"json"` feature.
    #[cfg(feature = "json")]
    #[inline]
    pub fn json<T: Serialize>(self, data: &T) -> Result<Request<Full<Bytes>>, Error> {
        const MIME_JSON: HeaderValue = HeaderValue::from_static("application/json");

        let body = serde_json::to_vec(data)?;
        self.ensure_content_type(MIME_JSON)
            .body(Full::new(Bytes::from(body)))
    }

    /// Sends the request with an empty body.
    #[inline]
    pub async fn send(self) -> Result<Response, Error> {
        self.body(Empty::<Bytes>::new())?.send().await
    }
}

impl TryFrom<RequestBuilder> for http::Request<Empty<Bytes>> {
    type Error = Error;

    #[inline]
    fn try_from(builder: RequestBuilder) -> Result<Self, Self::Error> {
        Ok(builder.build()?.inner)
    }
}

/// Represents an HTTP/3 request constructed by [`RequestBuilder`].
#[must_use]
#[derive(Debug, Clone)]
pub struct Request<T> {
    inner: http::Request<T>,
    client: IrohH3Client,
}

impl<B> Request<B>
where
    B: Body,
    http::Error: From<B::Error>,
{
    /// Sends this request using the associated [`IrohH3Client`].
    #[inline]
    pub async fn send(self) -> Result<Response, Error> {
        let response = self.client.send(self.inner).await?;
        Ok(response)
    }
}

impl<B> From<Request<B>> for http::Request<B> {
    fn from(value: Request<B>) -> Self {
        value.inner
    }
}
