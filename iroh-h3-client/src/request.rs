use bytes::Bytes;
use http::request::Builder;
use http_body::Body;
use http_body_util::Empty;
#[cfg(feature = "json")]
use http_body_util::Full;
#[cfg(feature = "json")]
use serde::Serialize;

use crate::{IrohH3Client, error::Error, response::Response};

/// A builder for constructing HTTP/3 requests.
///
/// This struct provides methods to configure and send HTTP/3 requests using the [`IrohH3Client`].
/// It allows setting headers, extensions, and the request body.
#[derive(Debug)]
#[must_use]
pub struct RequestBuilder {
    pub(crate) inner: Builder,
    pub(crate) client: IrohH3Client,
}

impl RequestBuilder {
    /// Adds an extension to the request.
    ///
    /// Extensions are arbitrary data that can be associated with the request.
    #[inline]
    pub fn extension<T>(mut self, extension: T) -> Self
    where
        T: Clone + std::any::Any + Send + Sync + 'static,
    {
        self.inner = self.inner.extension(extension);
        self
    }

    /// Adds a header to the request.
    ///
    /// # Parameters
    /// - `key`: The name of the header.
    /// - `value`: The value of the header.
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

    /// Sets the body of the request and finalizes the builder.
    ///
    /// # Parameters
    /// - `body`: The body of the request.
    ///
    /// # Returns
    /// A [`Request`] object containing the configured request.
    ///
    /// # Errors
    /// Returns an [`Error`] if the request cannot be constructed.
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

    /// Sets the body of the request to JSON-serialized data and finalizes the builder.
    ///
    /// # Parameters
    /// - `data`: A reference to the value to serialize as JSON.
    ///
    /// # Returns
    /// A [`Request`] object containing the configured request with a JSON body.
    ///
    /// # Errors
    /// Returns an [`Error`] if the data cannot be serialized to JSON or if the request cannot be constructed.
    #[cfg(feature = "json")]
    pub fn json<T: Serialize>(self, data: &T) -> Result<Request<Full<Bytes>>, Error> {
        use http::{HeaderValue, header::CONTENT_TYPE};

        use crate::error::JsonError;
        const MIME_JSON: HeaderValue = HeaderValue::from_static("application/json");

        let request = self
            .inner
            .header(CONTENT_TYPE, MIME_JSON)
            .body(serde_json::to_vec(data).map_err(JsonError::from)?.into())?;

        Ok(Request {
            inner: request,
            client: self.client,
        })
    }

    /// Sends the request with an empty body.
    ///
    /// # Returns
    /// A [`Response`] object representing the server's response.
    ///
    /// # Errors
    /// Returns an [`Error`] if the request fails to send or the response cannot be received.
    #[inline]
    pub async fn send(self) -> Result<Response, Error> {
        self.body(Empty::<Bytes>::new())?.send().await
    }
}

impl TryFrom<RequestBuilder> for http::Request<Empty<Bytes>> {
    type Error = Error;

    /// Converts the builder into an HTTP request with an empty body.
    ///
    /// # Errors
    /// Returns an [`Error`] if the request cannot be constructed.
    #[inline]
    fn try_from(builder: RequestBuilder) -> Result<Self, Self::Error> {
        let request = builder.inner.body(Empty::<Bytes>::new())?;
        Ok(request)
    }
}

/// Represents an HTTP/3 request constructed using the [`RequestBuilder`].
///
/// This struct encapsulates the request and provides a method to send it
/// using the associated [`IrohH3Client`].
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
    /// Sends the HTTP/3 request and returns the server's response.
    ///
    /// # Returns
    /// A [`Response`] object representing the server's response.
    ///
    /// # Errors
    /// Returns an [`Error`] if the request fails to send or the response cannot be received.
    #[inline]
    pub async fn send(self) -> Result<Response, Error> {
        let response = self.client.send(self.inner).await?;
        Ok(response)
    }
}
