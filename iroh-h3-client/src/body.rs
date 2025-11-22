use bytes::Bytes;
use futures::StreamExt;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use tracing::instrument;

use crate::error::Error;

#[derive(Debug, Default)]
pub struct Body {
    inner: Inner,
}

impl Body {
    pub fn empty() -> Self {
        Self {
            inner: Inner::Bytes(Bytes::new()),
        }
    }

    pub fn bytes(bytes: Bytes) -> Self {
        Self {
            inner: Inner::Bytes(bytes),
        }
    }

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

    pub fn into_stream(self) -> BoxBody<Bytes, Error> {
        match self.inner {
            Inner::Stream(box_body) => box_body,
            Inner::Bytes(bytes) => Full::new(bytes).map_err(Error::from).boxed(),
        }
    }

    pub fn try_clone(&self) -> Option<Self> {
        match &self.inner {
            Inner::Bytes(bytes) => Some(Self::bytes(bytes.clone())),
            Inner::Stream(_) => None,
        }
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
