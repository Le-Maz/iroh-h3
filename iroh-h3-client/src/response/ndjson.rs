//! # NDJSON Streaming Parser
//!
//! This module provides [`NdjsonStream`], a [`futures::Stream`] of values
//! obtained from parsing newline-delimited JSON (NDJSON) received through an
//! HTTP response body. NDJSON is commonly used by streaming APIs that emit a
//! large or unbounded sequence of independent JSON objects, one per line.
//!
//! `NdjsonStream<T>` reconstructs each `\n`-terminated JSON line from the raw
//! byte frames provided by the HTTP body, even when a single line spans
//! multiple frames or when a frame contains multiple NDJSON objects. Each
//! complete line is deserialized into a value of type `T` using `serde_json`
//! and yielded as soon as available.
//!
//! This is ideal for streaming analytics endpoints, real-time logs, and
//! long-running model inference endpoints that produce many JSON objects
//! incrementally instead of a single aggregate response.
//!
//! ## Characteristics
//!
//! - Handles arbitrary chunking and frame boundaries.
//! - Reassembles complete `\n`-terminated lines.
//! - Deserializes each line using [`serde_json::from_slice`].  
//! - Emits values as soon as they are complete (no buffering the whole body).
//! - Propagates both HTTP body errors and JSON parsing errors.
//!
//! ## Requirements
//!
//! - Each JSON object **must** fit on a single line ending with `\n`.
//! - Partial final lines without `\n` at EOF are **silently discarded**
//!   (standard NDJSON behavior).
//!
//! For streaming formats other than NDJSON (e.g., SSE), use the corresponding
//! modules in this crate.

use std::{
    collections::VecDeque,
    pin::{Pin, pin},
    task::{Context, Poll, ready},
};

use bytes::{Buf, Bytes};
use futures::Stream;
use http_body::Body;
use http_body_util::combinators::BoxBody;
use serde::de::DeserializeOwned;
use serde_json::from_slice;
use tracing::instrument;

use crate::{error::Error, response::Response};

/// A streaming **NDJSON (newline-delimited JSON)** parser.
///
/// This parser accepts a byte stream (typically an HTTP response body),
/// reconstructs `\n`-terminated lines across arbitrary frame boundaries,
/// and deserializes each line into a strongly typed value `T` using
/// `serde_json`.
///
/// ## How it works
///
/// - Bytes accumulate in an internal buffer until a newline is encountered.
/// - The bytes of that line are copied into a scratch buffer.
/// - The line is parsed as JSON into `T`.
/// - The result is pushed into an internal queue.
/// - [`poll_next`](futures::Stream::poll_next) returns items from this queue.
///
/// ## Type Parameter
///
/// `T` must implement [`serde::de::DeserializeOwned`].
pub struct NdjsonStream<T> {
    /// The underlying HTTP body stream producing `Bytes` frames.
    body: BoxBody<Bytes, Error>,

    /// Accumulates incoming bytes until one or more newline-terminated lines
    /// can be extracted.
    buffer: VecDeque<u8>,

    /// A scratch buffer reused for constructing a single NDJSON line before
    /// attempting JSON deserialization.
    line_buf: Vec<u8>,

    /// A FIFO queue of parsed results waiting to be yielded to the caller.
    ///
    /// Multiple JSON objects may be parsed from a single frame, and parsing
    /// produces results synchronously, whereas the `Stream` interface yields
    /// items one at a time. This queue decouples these behaviors.
    queue: VecDeque<Result<T, Error>>,

    /// Marker for the generic type.
    _marker: std::marker::PhantomData<T>,
}

impl<T> NdjsonStream<T>
where
    T: DeserializeOwned,
{
    /// Creates a new NDJSON stream from an HTTP response.
    ///
    /// The response body is consumed as a stream of `Bytes` frames.
    pub fn new(response: Response) -> Self {
        Self {
            body: response.body.into_stream(),
            buffer: VecDeque::with_capacity(256),
            line_buf: Vec::with_capacity(256),
            queue: VecDeque::with_capacity(16),
            _marker: std::marker::PhantomData,
        }
    }

    /// Constructs an `NdjsonStream` from any arbitrary byte-producing stream.
    ///
    /// This is intended for testing and avoids needing a full `Response`.
    #[cfg(test)]
    pub(crate) fn from_stream<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, Error>> + Send + Sync + 'static,
    {
        use futures::StreamExt;
        use http_body::Frame;
        use http_body_util::StreamBody;

        let stream = stream.map(|buf| Ok(Frame::data(buf?)));

        Self {
            body: BoxBody::new(StreamBody::new(stream)),
            buffer: VecDeque::with_capacity(256),
            line_buf: Vec::with_capacity(256),
            queue: VecDeque::with_capacity(16),
            _marker: std::marker::PhantomData,
        }
    }

    /// Appends raw bytes from a frame, extracts any completed NDJSON lines,
    /// and attempts to deserialize them into values of type `T`.
    ///
    /// Deserialization results (both `Ok` and `Err`) are pushed into
    /// [`queue`](Self::queue) for later consumption by the stream interface.
    fn append_frame(&mut self, mut frame: impl Buf) {
        let old_len = self.buffer.len();

        // Copy incoming bytes into the main buffer
        while frame.has_remaining() {
            let chunk = frame.chunk();
            self.buffer.extend(chunk);
            frame.advance(chunk.len());
        }

        let mut processed = 0;

        // Scan for newline-delimited NDJSON lines
        for pos in old_len..self.buffer.len() {
            if self.buffer[pos] == b'\n' {
                let start = processed;
                let end = pos;

                // Copy the bytes for the NDJSON line into line_buf
                let mut buf = std::mem::take(&mut self.line_buf);
                buf.clear();
                buf.reserve(end - start);

                let (s1, s2) = self.buffer.as_slices();
                if end <= s1.len() {
                    buf.extend_from_slice(&s1[start..end]);
                } else if start >= s1.len() {
                    let s2_start = start - s1.len();
                    let s2_end = end - s1.len();
                    buf.extend_from_slice(&s2[s2_start..s2_end]);
                } else {
                    buf.extend_from_slice(&s1[start..]);
                    buf.extend_from_slice(&s2[..(end - s1.len())]);
                }

                // Attempt JSON deserialization
                let value = from_slice::<T>(&buf)
                    .map_err(From::from)
                    .map_err(Error::ResponseValidation);

                self.queue.push_back(value);

                // Reuse the buffer for later lines
                buf.clear();
                self.line_buf = buf;

                processed = pos + 1;
            }
        }

        // Discard processed bytes
        self.buffer.drain(0..processed);
    }
}

/// Marks `NdjsonStream<T>` as `Unpin` regardless of whether `T` is `Unpin`.
///
/// This is safe because `NdjsonStream` does not store any pinned projections
/// of values of type `T`, nor does it move any internal futures or self-referential
/// structures that depend on pinning. The type `T` is only ever created as an
/// owned value inside the `queue` and is never borrowed across `.poll_next()`.
///
/// By implementing `Unpin` unconditionally, an `NdjsonStream<T>` can be moved
/// freely after being pinned, and can be used with combinators or executors
/// that require `Unpin`, even for types `T` that are not themselves `Unpin`.
impl<T> Unpin for NdjsonStream<T> {}

impl<T> Stream for NdjsonStream<T>
where
    T: DeserializeOwned,
{
    type Item = Result<T, Error>;

    /// Attempts to produce the next parsed NDJSON value.
    ///
    /// If previously parsed values are waiting in the internal queue, they
    /// are returned immediately. Otherwise, the underlying HTTP body is
    /// polled for more data, which may produce zero or more new parsed items.

    #[instrument(skip(self, cx))]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(item) = self.queue.pop_front() {
            return Poll::Ready(Some(item));
        }

        let body = pin!(&mut self.body);
        let poll = Body::poll_frame(body, cx);
        let frame_result = ready!(poll);

        let frame = match frame_result {
            Some(Ok(frame)) => frame,
            Some(Err(err)) => return Poll::Ready(Some(Err(err))),
            None => return Poll::Ready(None), // EOF
        };

        // Only handle DATA frames
        let Ok(data) = frame.into_data() else {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        };

        // Scan for newline-delimited JSON
        self.append_frame(data);

        if let Some(item) = self.queue.pop_front() {
            return Poll::Ready(Some(item));
        }

        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::{StreamExt, stream};
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Row {
        id: u64,
        #[serde(default)]
        name: String,
    }

    fn ok(bytes: &'static [u8]) -> Result<Bytes, Error> {
        Ok(Bytes::from_static(bytes))
    }

    #[tokio::test]
    async fn simple_row() {
        let frames = stream::iter(vec![ok(b"{\"id\":1}\n")]);

        let mut nd = NdjsonStream::<Row>::from_stream(frames);
        let row = nd.next().await.unwrap().unwrap();

        assert_eq!(
            row,
            Row {
                id: 1,
                name: "".into()
            }
        );
        assert!(nd.next().await.is_none());
    }

    #[tokio::test]
    async fn split_frames_recombine() {
        let frames = stream::iter(vec![ok(b"{\"id\""), ok(b":2}\n")]);

        let mut nd = NdjsonStream::<Row>::from_stream(frames);
        let row = nd.next().await.unwrap().unwrap();

        assert_eq!(row.id, 2);
    }

    #[tokio::test]
    async fn multiple_rows_in_one_frame() {
        let frames = stream::iter(vec![ok(b"{\"id\":1}\n{\"id\":2}\n")]);

        let mut nd = NdjsonStream::<Row>::from_stream(frames);

        let r1 = nd.next().await.unwrap().unwrap();
        let r2 = nd.next().await.unwrap().unwrap();

        assert_eq!(r1.id, 1);
        assert_eq!(r2.id, 2);
    }

    #[tokio::test]
    async fn unknown_fields_ignored() {
        let frames = stream::iter(vec![ok(b"{\"id\":3,\"foo\":123}\n")]);

        let mut nd = NdjsonStream::<Row>::from_stream(frames);
        let row = nd.next().await.unwrap().unwrap();

        assert_eq!(row.id, 3);
    }

    #[tokio::test]
    async fn malformed_json_propagates_error() {
        let frames = stream::iter(vec![ok(b"{bad json}\n")]);

        let mut nd = NdjsonStream::<Row>::from_stream(frames);

        let err = nd.next().await.unwrap().unwrap_err();

        match err {
            Error::ResponseValidation(_) => {}
            _ => panic!("expected ResponseValidation"),
        }
    }

    #[tokio::test]
    async fn incomplete_line_on_eof_discarded() {
        let frames = stream::iter(vec![
            ok(b"{\"id\":10"), // missing newline
        ]);

        let mut nd = NdjsonStream::<Row>::from_stream(frames);

        assert!(nd.next().await.is_none());
    }
}
