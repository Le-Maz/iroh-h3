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
#[cfg(feature = "json")]
use futures::Stream;
use http::request::Builder;
use http::{HeaderValue, header::CONTENT_TYPE};
#[cfg(feature = "json")]
use serde::Serialize;
use tracing::instrument;

use crate::IrohH3Client;
use crate::body::Body;
use crate::middleware::Service;
use crate::{error::Error, response::Response};

/// Alias to the Request with its client set to IrohH3Client
pub type ClientRequest = Request<IrohH3Client>;

/// A builder for constructing HTTP/3 requests.
///
/// This struct provides methods to configure and send HTTP/3 requests using
/// the [`IrohH3Client`]. It allows setting headers, extensions, and the
/// request body in various formats.
#[derive(Debug)]
#[must_use]
pub struct RequestBuilder<C: Service> {
    pub(crate) inner: Builder,
    pub(crate) client: C,
}

impl<C: Service> RequestBuilder<C> {
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
    pub fn body(self, body: Body) -> Result<Request<C>, Error> {
        let request = self.inner.body(body)?;
        Ok(Request {
            inner: request,
            client: self.client,
        })
    }

    /// Builds a request with an empty body.
    #[inline]
    pub fn build(self) -> Result<Request<C>, Error> {
        self.body(Body::empty())
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
    pub fn text(self, text: impl AsRef<str>) -> Result<Request<C>, Error> {
        const MIME_TEXT: HeaderValue = HeaderValue::from_static("text/plain; charset=utf-8");

        let body_bytes = Bytes::copy_from_slice(text.as_ref().as_bytes());
        self.ensure_content_type(MIME_TEXT)
            .body(Body::bytes(body_bytes))
    }

    /// Sets the request body to the given binary bytes.
    ///
    /// Automatically sets the `Content-Type` header to
    /// `"application/octet-stream"` **if it is not already set**.
    ///
    /// # Errors
    /// Returns an [`Error`] if the request cannot be constructed.
    #[inline]
    pub fn bytes(self, bytes: impl Into<Bytes>) -> Result<Request<C>, Error> {
        const MIME_BIN: HeaderValue = HeaderValue::from_static("application/octet-stream");

        self.ensure_content_type(MIME_BIN)
            .body(Body::bytes(bytes.into()))
    }

    /// Sets the body of the request to JSON-serialized data.
    ///
    /// Automatically sets the `Content-Type` header to
    /// `"application/json"` **if it is not already set**.
    ///
    /// Requires the `"json"` feature.
    #[cfg(feature = "json")]
    #[inline]
    pub fn json<T: Serialize>(self, data: &T) -> Result<Request<C>, Error> {
        const MIME_JSON: HeaderValue = HeaderValue::from_static("application/json");

        let body = serde_json::to_vec(data).map_err(|err| Error::RequestValidation(err.into()))?;
        self.ensure_content_type(MIME_JSON)
            .body(Body::bytes(Bytes::from(body)))
    }

    /// Sets the body of the request to NDJSON-serialized data from a stream.
    ///
    /// NDJSON (Newline-Delimited JSON) is a format where each JSON object is
    /// written on a single line, separated by `\n`. This method consumes a
    /// [`Stream`] of serializable items, serializes each one, and appends a
    /// newline between them.
    ///
    /// The `Content-Type` header is automatically set to `"application/x-ndjson"`
    /// if it is not already present.
    ///
    /// Requires the `"json"` feature.
    #[cfg(feature = "json")]
    #[inline]
    pub fn ndjson<T: Serialize>(
        self,
        data: impl Stream<Item = T> + Unpin + Send + Sync + 'static,
    ) -> Result<Request<C>, Error> {
        use futures::stream::StreamExt;
        use http_body_util::{StreamBody, combinators::BoxBody};

        const MIME_NDJSON: HeaderValue = HeaderValue::from_static("application/x-ndjson");

        let stream = data.map(|item| {
            use http_body::Frame;

            let mut buffer =
                serde_json::to_vec(&item).map_err(|err| Error::RequestValidation(err.into()))?;
            buffer.push(b'\n');

            Ok::<_, Error>(Frame::data(Bytes::from(buffer)))
        });

        let body = BoxBody::new(StreamBody::new(stream));

        self.ensure_content_type(MIME_NDJSON).body(Body::from(body))
    }

    /// Sends the request with an empty body.
    #[inline]
    #[instrument(skip(self))]
    pub async fn send(self) -> Result<Response, Error> {
        self.build()?.send().await
    }
}

/// Represents an HTTP/3 request constructed by [`RequestBuilder`].
#[must_use]
#[derive(Debug)]
pub struct Request<C: Service> {
    inner: http::Request<Body>,
    client: C,
}

impl<C: Service> Request<C> {
    /// Sends this request using the associated [`IrohH3Client`].
    #[inline]
    #[instrument(skip(self))]
    pub async fn send(self) -> Result<Response, Error> {
        let response = self.client.handle(self.inner).await?;
        Ok(response.into())
    }
}

impl<C: Service> From<Request<C>> for http::Request<Body> {
    fn from(value: Request<C>) -> Self {
        value.inner
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        body::Body,
        error::Error,
        middleware::{MockService, Service},
        request::{Request, RequestBuilder},
    };
    use http::{HeaderValue, Request as HttpRequest, Response as HttpResponse};
    use std::sync::Arc;
    // use tokio::sync::Mutex;
    use n0_future::sync::Mutex; // unifies wasm/tokio usage

    type SharedMockService = Arc<Mutex<MockService>>;

    impl Service for SharedMockService {
        async fn handle(&self, request: HttpRequest<Body>) -> Result<HttpResponse<Body>, Error> {
            self.lock().await.handle(request).await
        }
    }

    fn builder_with_mock() -> (RequestBuilder<SharedMockService>, SharedMockService) {
        let mock_service = Arc::new(Mutex::new(MockService::new()));
        let builder = RequestBuilder {
            inner: http::Request::builder(),
            client: mock_service.clone(),
        };
        (builder, mock_service)
    }

    async fn verify_body(
        mut request: Request<SharedMockService>,
        predicate: impl Fn(&[u8]) -> bool,
    ) {
        let body_bytes = request.inner.body_mut().take().into_bytes().await.unwrap();
        assert!(predicate(&body_bytes));
    }

    #[tokio::test]
    async fn set_header() {
        let (builder, _mock) = builder_with_mock();
        let builder = builder.header("X-Test", "value");
        let req = builder.build().unwrap();
        assert_eq!(
            req.inner.headers().get("X-Test"),
            Some(&HeaderValue::from_static("value"))
        );
    }

    #[tokio::test]
    async fn add_extension() {
        #[derive(Clone)]
        struct MyExtension;
        let (builder, _mock) = builder_with_mock();
        let builder = builder.extension(MyExtension);
        let req = builder.build().unwrap();
        assert!(req.inner.extensions().get::<MyExtension>().is_some());
    }

    #[tokio::test]
    async fn text_body_sets_content_type() {
        let (builder, _mock) = builder_with_mock();
        let req = builder.text("hello world").unwrap();
        assert_eq!(
            req.inner.headers().get("content-type"),
            Some(&HeaderValue::from_static("text/plain; charset=utf-8"))
        );
        verify_body(req, |b| b == b"hello world").await;
    }

    #[tokio::test]
    async fn bytes_body_sets_content_type() {
        let (builder, _mock) = builder_with_mock();
        let data = b"abc".to_vec();
        let req = builder.bytes(data.clone()).unwrap();
        assert_eq!(
            req.inner.headers().get("content-type"),
            Some(&HeaderValue::from_static("application/octet-stream"))
        );
        verify_body(req, |b| b == b"abc").await;
    }

    #[tokio::test]
    async fn build_empty_body() {
        let (builder, _mock) = builder_with_mock();
        let req = builder.build().unwrap();
        verify_body(req, |b| b.is_empty()).await;
    }

    #[tokio::test]
    async fn send_request_calls_service() {
        let (builder, mock_service) = builder_with_mock();

        {
            let mut mock = mock_service.lock().await;
            mock.expect_handle().times(1).returning(|mut req| {
                Box::pin(async move {
                    Ok(HttpResponse::builder()
                        .status(200)
                        .body(req.body_mut().take())
                        .unwrap())
                })
            });
        }

        let req = builder.text("hello").unwrap();
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.bytes().await.unwrap().to_vec(), b"hello");
    }

    #[tokio::test]
    async fn ensure_content_type_no_overwriting() {
        let (builder, _mock) = builder_with_mock();
        let builder = builder.header("content-type", "custom/type");
        let req = builder.text("hello").unwrap();
        assert_eq!(
            req.inner.headers().get("content-type"),
            Some(&HeaderValue::from_static("custom/type"))
        );
        verify_body(req, |b| b == b"hello").await;
    }

    #[cfg(feature = "json")]
    #[tokio::test]
    async fn send_request_with_ndjson() {
        use futures::stream;
        use serde_json::Value;

        let (builder, _mock) = builder_with_mock();

        let items = vec![serde_json::json!({"a": 1}), serde_json::json!({"b": 2})];
        let item_stream = stream::iter(items.clone());

        let req = builder.ndjson(item_stream).unwrap();

        assert_eq!(
            req.inner.headers().get("content-type"),
            Some(&http::HeaderValue::from_static("application/x-ndjson"))
        );

        verify_body(req, |body_bytes| {
            let body_str = std::str::from_utf8(&body_bytes).expect("body is valid UTF-8");
            let lines: Vec<&str> = body_str.lines().collect();

            let parsed: Vec<Value> = lines
                .into_iter()
                .map(|line| serde_json::from_str(line).expect("valid JSON line"))
                .collect();

            parsed == items
        })
        .await;
    }
}
