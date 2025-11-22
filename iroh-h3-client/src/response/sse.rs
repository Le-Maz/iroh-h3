//! Server-Sent Events (SSE) stream parser.
//!
//! This module provides [`SseStream`], a [`futures::Stream`] implementation
//! that incrementally parses an HTTP/3 response body into individual
//! Server-Sent Events according to the SSE specification.
//!
//! It handles:
//! - buffering arbitrary frame boundaries,
//! - reconstructing `\n`-delimited SSE lines,
//! - grouping lines into events (separated by blank lines),
//! - extracting fields such as `data:`, `id:`, and `event:`,
//! - maintaining `last_event_id` semantics.
//!
//! The output item of the stream is [`Result<SseEvent, Error>`].

use std::{
    collections::LinkedList,
    mem::swap,
    pin::{Pin, pin},
    task::{Context, Poll, ready},
};

use bytes::{Buf, Bytes};
use futures::Stream;
use http_body::Body;
use http_body_util::combinators::BoxBody;
use tracing::instrument;

use crate::{error::Error, response::Response};

/// A streaming parser for Server-Sent Events.
///
/// `SseStream` consumes an underlying [`Response`] that yields raw byte
/// frames. Frames may split SSE lines arbitrarily, so the stream buffers
/// data internally until complete lines and complete events can be
/// constructed.
///
/// An SSE event is emitted when a blank line is encountered. If the
/// underlying response ends and no complete event is pending, `None` is
/// returned.
pub struct SseStream {
    /// The HTTP/3 response body.
    body: BoxBody<Bytes, Error>,

    /// A buffer of unprocessed raw bytes that may contain partial lines.
    buffer: Vec<u8>,

    /// Completed lines waiting to be grouped into events.
    lines: LinkedList<String>,

    /// Iterator cursor over `lines` to find the next event boundary.
    line_cursor: usize,

    /// Tracks the most recent event ID sent by the server.
    last_event_id: Option<String>,
}

impl SseStream {
    /// Create a new SSE stream from an HTTP response.
    pub fn new(response: Response) -> Self {
        SseStream {
            body: response.body.into_stream(),
            buffer: Vec::with_capacity(256),
            lines: LinkedList::new(),
            line_cursor: 0,
            last_event_id: None,
        }
    }

    /// Create an SSE stream directly from any byte stream.
    ///
    /// This allows unit tests to inject arbitrary frames.
    #[cfg(test)]
    pub(crate) fn from_stream<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, Error>> + Send + Sync + 'static,
    {
        use futures::StreamExt;
        use http_body::Frame;
        use http_body_util::StreamBody;

        let stream = stream.map(|buf| Ok(Frame::data(buf?)));
        SseStream {
            body: BoxBody::new(StreamBody::new(stream)),
            buffer: Vec::with_capacity(256),
            lines: LinkedList::new(),
            line_cursor: 0,
            last_event_id: None,
        }
    }

    /// Returns the last received SSE event ID, if any.
    pub fn last_event_id(&self) -> Option<&str> {
        self.last_event_id.as_deref()
    }

    /// Append a new frame of bytes into the internal buffer.
    ///
    /// This method merges the incoming frame into `self.buffer`, then
    /// extracts any newly completed lines (terminated by `\n`) and adds
    /// them into `self.lines`.
    fn append_frame(&mut self, mut frame: impl Buf) {
        let previous_length = self.buffer.len();
        while frame.has_remaining() {
            let chunk = frame.chunk();
            self.buffer.extend_from_slice(chunk);
            frame.advance(chunk.len());
        }
        let mut lines = self.take_complete_lines_since(previous_length);
        self.lines.append(&mut lines);
    }

    /// Extract all complete lines (ending with `\n`) from the buffer
    /// starting from `scan_from`.
    ///
    /// The extracted lines are removed from `self.buffer` and returned.
    fn take_complete_lines_since(&mut self, scan_from: usize) -> LinkedList<String> {
        let mut start = 0;
        let mut lines = LinkedList::new();

        for i in scan_from..self.buffer.len() {
            if self.buffer[i] != b'\n' {
                continue;
            }

            let line_bytes = &self.buffer[start..i];
            let line = String::from_utf8_lossy(line_bytes).to_string();

            lines.push_back(line);
            start = i + 1;
        }

        // Remove processed bytes.
        self.buffer.drain(0..start);
        lines
    }

    /// Attempt to advance the cursor and extract the next complete SSE event.
    ///
    /// An SSE event is defined as a block of one or more lines terminated
    /// by a blank line (`""`). Returns `None` if no complete event is
    /// available yet.
    fn advance_line_cursor(&mut self) -> Option<SseEvent> {
        let first_empty = self
            .lines
            .iter()
            .skip(self.line_cursor)
            .enumerate()
            .find_map(|(index, line)| if line.is_empty() { Some(index) } else { None });

        if first_empty.is_none() {
            self.line_cursor = self.lines.len();
        }

        let first_empty = first_empty?;
        let mut message_lines = self.lines.split_off(first_empty + 1);
        swap(&mut message_lines, &mut self.lines);
        self.line_cursor = 0;
        let event = self.build_event_from_lines(message_lines);
        Some(event)
    }

    /// Build an [`SseEvent`] from a list of SSE lines.
    ///
    /// Each line is parsed according to SSE field rules (`data:`, `id:`,
    /// `event:`). Lines beginning with unknown fields are ignored.
    fn build_event_from_lines(&mut self, message_lines: LinkedList<String>) -> SseEvent {
        let mut event = SseEvent::default();
        for line in message_lines {
            let mut parts = line.splitn(2, ':');
            let name = parts.next().unwrap_or("");
            let value = parts.next().map(|v| v.trim_start()).unwrap_or("");

            match name {
                "data" => {
                    event.data.push_str(value);
                    event.data.push('\n');
                }
                "id" => {
                    self.last_event_id = Some(value.to_owned());
                    event.id = Some(value.to_owned());
                }
                "event" => {
                    event.event = Some(value.to_owned());
                }
                _ => {}
            }
        }
        // Remove trailing newline.
        event.data.pop();
        event
    }
}

impl Stream for SseStream {
    type Item = Result<SseEvent, Error>;

    /// Poll for the next SSE event.
    ///
    /// Attempts to parse and return a complete event if possible.
    /// Otherwise waits for more data from the underlying response's
    /// stream.
    #[instrument(skip(self, cx))]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Emit any buffered complete event first.
        if let Some(event) = self.advance_line_cursor() {
            return Poll::Ready(Some(Ok(event)));
        }

        // Poll the underlying response body stream.
        let body = pin!(&mut self.body);
        let data_poll = Body::poll_frame(body, cx);
        let data_result = ready!(data_poll);
        let frame = match data_result {
            Some(Ok(frame)) => frame,
            Some(Err(error)) => {
                return Poll::Ready(Some(Err(error)));
            }
            _ if self.buffer.is_empty() && self.lines.is_empty() => {
                return Poll::Ready(None);
            }
            _ => {
                if let Some(event) = self.advance_line_cursor() {
                    return Poll::Ready(Some(Ok(event)));
                }
                return Poll::Ready(None);
            }
        };

        // Process the new frame.
        let Ok(data) = frame.into_data() else {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        };
        self.append_frame(data);

        if let Some(event) = self.advance_line_cursor() {
            return Poll::Ready(Some(Ok(event)));
        };

        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// A single Server-Sent Event.
///
/// Fields correspond to standard SSE fields:
/// - `id` — event ID used for reconnection,
/// - `event` — event type,
/// - `data` — textual payload (may contain newlines).
#[derive(Debug, Default)]
pub struct SseEvent {
    id: Option<String>,
    event: Option<String>,
    data: String,
}

impl SseEvent {
    /// Get the event ID, if supplied.
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Get the event type name, if supplied.
    pub fn event(&self) -> Option<&str> {
        self.event.as_deref()
    }

    /// Get the event data payload.
    pub fn data(&self) -> &str {
        &self.data
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use bytes::Bytes;
    use futures::{StreamExt, stream};

    fn ok_bytes(bytes: &'static [u8]) -> Result<Bytes, Error> {
        Ok(Bytes::from_static(bytes))
    }

    // -------------------------------
    // Basic event parsing
    // -------------------------------

    #[tokio::test]
    async fn simple_event() {
        let frames = stream::iter(vec![ok_bytes(b"data: hello\n\n")]);

        let mut sse = SseStream::from_stream(frames);

        let event = sse.next().await.unwrap().unwrap();

        assert_eq!(event.data(), "hello");
        assert_eq!(event.id(), None);
        assert_eq!(event.event(), None);

        // stream ends
        assert!(sse.next().await.is_none());
    }

    // -------------------------------
    // Split frames should recombine
    // -------------------------------

    #[tokio::test]
    async fn split_frames_into_one_event() {
        let frames = stream::iter(vec![
            ok_bytes(b"da"),
            ok_bytes(b"ta: hel"),
            ok_bytes(b"lo\n"),
            ok_bytes(b"\n"),
        ]);

        let mut sse = SseStream::from_stream(frames);

        let event = sse.next().await.unwrap().unwrap();
        assert_eq!(event.data(), "hello");
    }

    // -------------------------------
    // Multiple events in a single frame
    // -------------------------------

    #[tokio::test]
    async fn multiple_events_one_frame() {
        let frames = stream::iter(vec![ok_bytes(b"data: one\n\ndata: two\n\n")]);

        let mut sse = SseStream::from_stream(frames);

        let ev1 = sse.next().await.unwrap().unwrap();
        let ev2 = sse.next().await.unwrap().unwrap();

        assert_eq!(ev1.data(), "one");
        assert_eq!(ev2.data(), "two");
    }

    // -------------------------------
    // id: field
    // -------------------------------

    #[tokio::test]
    async fn event_id_updates_last_event_id() {
        let frames = stream::iter(vec![
            ok_bytes(b"id: 123\n"),
            ok_bytes(b"data: x\n"),
            ok_bytes(b"\n"),
            ok_bytes(b"id: 456\n"),
            ok_bytes(b"data: y\n\n"),
        ]);

        let mut sse = SseStream::from_stream(frames);

        let e1 = sse.next().await.unwrap().unwrap();
        assert_eq!(e1.id(), Some("123"));
        assert_eq!(sse.last_event_id(), Some("123"));

        let e2 = sse.next().await.unwrap().unwrap();
        assert_eq!(e2.id(), Some("456"));
        assert_eq!(sse.last_event_id(), Some("456"));
    }

    // -------------------------------
    // event: field
    // -------------------------------

    #[tokio::test]
    async fn event_type_parsed() {
        let frames = stream::iter(vec![
            ok_bytes(b"event: greeting\n"),
            ok_bytes(b"data: hi\n\n"),
        ]);

        let mut sse = SseStream::from_stream(frames);
        let event = sse.next().await.unwrap().unwrap();

        assert_eq!(event.event(), Some("greeting"));
        assert_eq!(event.data(), "hi");
    }

    // -------------------------------
    // Multiline data
    // -------------------------------

    #[tokio::test]
    async fn multiple_data_lines() {
        let frames = stream::iter(vec![
            ok_bytes(b"data: a\n"),
            ok_bytes(b"data: b\n"),
            ok_bytes(b"data: c\n\n"),
        ]);

        let mut sse = SseStream::from_stream(frames);
        let event = sse.next().await.unwrap().unwrap();

        assert_eq!(event.data(), "a\nb\nc");
    }

    // -------------------------------
    // Ignore unknown fields
    // -------------------------------

    #[tokio::test]
    async fn ignore_unknown_fields() {
        let frames = stream::iter(vec![ok_bytes(b"foo: bar\n"), ok_bytes(b"data: hi\n\n")]);

        let mut sse = SseStream::from_stream(frames);
        let event = sse.next().await.unwrap().unwrap();

        assert_eq!(event.data(), "hi");
    }

    // -------------------------------
    // Blank event should still produce event
    // -------------------------------

    #[tokio::test]
    async fn blank_event() {
        let frames = stream::iter(vec![
            ok_bytes(b"\n"), // immediate blank event
        ]);

        let mut sse = SseStream::from_stream(frames);

        let event = sse.next().await.unwrap().unwrap();
        assert_eq!(event.data(), "");
        assert_eq!(event.id(), None);
        assert_eq!(event.event(), None);
    }

    // -------------------------------
    // Trailing newline removed
    // -------------------------------

    #[tokio::test]
    async fn trailing_newline_removed() {
        let frames = stream::iter(vec![
            ok_bytes(b"data: x\n"),
            ok_bytes(b"data: y\n"),
            ok_bytes(b"\n"),
        ]);

        let mut sse = SseStream::from_stream(frames);
        let event = sse.next().await.unwrap().unwrap();

        assert_eq!(event.data(), "x\ny");
    }

    // -------------------------------
    // Stream ends mid-event → no event emitted
    // -------------------------------

    #[tokio::test]
    async fn incomplete_event_on_eof_is_discarded() {
        let frames = stream::iter(vec![
            ok_bytes(b"data: incomplete"), // never ends with "\n\n"
        ]);

        let mut sse = SseStream::from_stream(frames);

        assert!(sse.next().await.is_none());
        assert_eq!(sse.last_event_id(), None);
    }

    // -------------------------------
    // Error propagation
    // -------------------------------

    #[tokio::test]
    async fn propagate_stream_error() {
        let err = Arc::new(Error::Other("boom".to_string()));

        let frames = stream::iter(vec![
            Ok(Bytes::from_static(b"data: ok\n\n")),
            Err(err.clone().into()),
        ]);

        let mut sse = SseStream::from_stream(frames);

        // first event is okay
        let first = sse.next().await.unwrap().unwrap();
        assert_eq!(first.data(), "ok");

        // next is error
        let second = sse.next().await.unwrap();
        assert!(second.is_err());
    }
}
