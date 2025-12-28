use bytes::Bytes;
use futures::StreamExt;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use tracing::instrument;

use crate::error::Error;

/// An HTTP/3 body that can be either a fixed set of bytes or a streaming body.
#[derive(Debug, Default)]
pub struct Body {
    inner: Inner,
}

impl Body {
    /// Create an empty body.
    pub fn empty() -> Self {
        Self {
            inner: Inner::Bytes(Bytes::new()),
        }
    }

    /// Create a body from the given bytes.
    pub fn bytes(bytes: Bytes) -> Self {
        Self {
            inner: Inner::Bytes(bytes),
        }
    }

    /// Consume the body and return its contents as bytes.
    #[instrument]
    pub async fn into_bytes(self) -> Result<Bytes, Error> {
        match self.inner {
            Inner::Bytes(bytes) => Ok(bytes),
            Inner::Stream(box_body) => {
                let mut stream = box_body.into_data_stream();
                let mut buffer = Vec::new();
                while let Some(chunk) = stream.next().await.transpose()? {
                    buffer.extend_from_slice(&chunk);
                }
                Ok(Bytes::from(buffer))
            }
        }
    }

    /// Consume the body and return it as a streaming body.
    pub fn into_stream(self) -> BoxBody<Bytes, Error> {
        match self.inner {
            Inner::Stream(box_body) => box_body,
            Inner::Bytes(bytes) => Full::new(bytes).map_err(Error::from).boxed(),
        }
    }

    /// Take the body, replacing it with an empty body.
    pub fn take(&mut self) -> Self {
        if let Inner::Bytes(bytes) = &self.inner {
            return Self::bytes(bytes.clone());
        };
        std::mem::take(self)
    }
}

impl<E> From<BoxBody<Bytes, E>> for Body
where
    E: Into<Error> + 'static,
{
    fn from(value: BoxBody<Bytes, E>) -> Self {
        Self {
            inner: Inner::Stream(value.map_err(E::into).boxed()),
        }
    }
}

#[derive(Debug)]
enum Inner {
    Bytes(Bytes),
    Stream(BoxBody<Bytes, Error>),
}

impl Default for Inner {
    fn default() -> Self {
        Self::Bytes(Bytes::default())
    }
}
