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
    collections::VecDeque,
    pin::{Pin, pin},
    task::{Context, Poll, ready},
};

use bytes::{Buf, Bytes};
use futures::Stream;
use http_body::Body;
use http_body_util::combinators::BoxBody;
use tracing::instrument;

use crate::{error::Error, response::Response};

/// A streaming Server-Sent Events (SSE) parser.
///
/// `SseStream` consumes an HTTP response body that yields `Bytes` frames
/// and incrementally parses them according to the SSE specification.
///
/// ## Key Features
///
/// - Handles arbitrary frame boundaries
/// - Reconstructs `\n`-terminated SSE lines
/// - Builds events incrementally as data arrives
/// - Supports `data:`, `id:`, and `event:` fields
/// - Maintains `last_event_id` semantics
///
/// This implementation avoids repeated scans and uses a `VecDeque<u8>`
/// for efficient front-draining of processed bytes.
pub struct SseStream {
    /// The HTTP response body being streamed.
    ///
    /// Each yielded frame contains raw bytes representing SSE text.
    body: BoxBody<Bytes, Error>,

    /// A deque of unprocessed raw bytes.
    ///
    /// Bytes are appended at the back and consumed from the front as
    /// complete lines (ending in `\n`) are detected.
    buffer: VecDeque<u8>,

    /// Scratch buffer used for assembling a single line.
    ///
    /// This buffer is reused for every parsed line to avoid repeated
    /// heap allocations. A fresh empty buffer is obtained via
    /// `mem::take(&mut self.line_buf)`, filled, processed, and then
    /// returned (empty) to `self.line_buf` for the next line.
    line_buf: Vec<u8>,

    /// The SSE event currently being constructed from incoming lines.
    ///
    /// Every non-blank line updates this pending event. A blank line
    /// completes the event and moves it into `ready_events`.
    pending_event: SseEvent,

    /// A queue of fully constructed SSE events ready to be emitted.
    ///
    /// The stream always yields events from this queue before polling the
    /// underlying HTTP body.
    ready_events: VecDeque<SseEvent>,

    /// The most recent `id:` field received from the server.
    ///
    /// This is updated by each event that includes an `id:` field.
    last_event_id: Option<String>,
}

impl SseStream {
    /// Create a new SSE stream from an HTTP response.
    pub fn new(response: Response) -> Self {
        Self {
            body: response.body.into_stream(),
            buffer: VecDeque::with_capacity(256),
            line_buf: Vec::with_capacity(256),
            pending_event: SseEvent::default(),
            ready_events: VecDeque::new(),
            last_event_id: None,
        }
    }

    /// Construct an `SseStream` from any byte stream (test-only).
    ///
    /// This allows injecting arbitrary `Bytes` frames for unit tests.
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
            pending_event: SseEvent::default(),
            ready_events: VecDeque::new(),
            last_event_id: None,
        }
    }

    /// Retrieve the most recently received SSE event ID, if any.
    pub fn last_event_id(&self) -> Option<&str> {
        self.last_event_id.as_deref()
    }

    /// Append raw bytes from a received frame into the internal buffer,
    /// extract complete `\n`-terminated lines, and process each line into
    /// SSE event fields.
    fn append_frame(&mut self, mut frame: impl Buf) {
        let old_len = self.buffer.len();

        while frame.has_remaining() {
            let chunk = frame.chunk();
            for &b in chunk {
                self.buffer.push_back(b);
            }
            frame.advance(chunk.len());
        }

        let mut processed = 0;
        for position in old_len..self.buffer.len() {
            if self.buffer[position] == b'\n' {
                let start = processed;
                let end = position;

                // obtain a temporary empty buffer
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

                let line = String::from_utf8_lossy(&buf);
                self.process_line(line.trim_end());

                // return buffer to struct for reuse
                buf.clear();
                self.line_buf = buf;

                processed = position + 1;
            }
        }

        for _ in 0..processed {
            self.buffer.pop_front();
        }
    }

    /// Process a single SSE text line.
    ///
    /// - A blank line completes the pending event.
    /// - Other lines update the pending event's fields.
    fn process_line(&mut self, line: &str) {
        // Blank line = end of event
        if line.is_empty() {
            self.ready_events
                .push_back(std::mem::take(&mut self.pending_event));
            return;
        }

        // Parse `name: value`
        let mut parts = line.splitn(2, ':');
        let name = parts.next().unwrap_or("");
        let value = parts.next().map(|v| v.trim_start()).unwrap_or("");

        match name {
            "data" => {
                if !self.pending_event.data.is_empty() {
                    self.pending_event.data.push('\n');
                }
                self.pending_event.data.push_str(value);
            }
            "id" => {
                self.pending_event.id = Some(value.to_owned());
                self.last_event_id = Some(value.to_owned());
            }
            "event" => {
                self.pending_event.event = Some(value.to_owned());
            }
            _ => {} // unknown fields ignored
        }
    }

    /// Retrieve the next completed event from the internal queue, if any.
    fn poll_ready_event(&mut self) -> Option<SseEvent> {
        self.ready_events.pop_front()
    }
}

impl Stream for SseStream {
    type Item = Result<SseEvent, Error>;

    /// Poll the SSE stream for the next event.
    ///
    /// Returns:
    /// - `Poll::Ready(Some(Ok(event)))` when an SSE event is available
    /// - `Poll::Ready(Some(Err(err)))` if the underlying stream fails
    /// - `Poll::Ready(None)` when the stream ends with no more events
    #[instrument(skip(self, cx))]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Emit any queued complete events immediately.
        if let Some(ev) = self.poll_ready_event() {
            return Poll::Ready(Some(Ok(ev)));
        }

        // Poll the underlying HTTP body stream.
        let body = pin!(&mut self.body);
        let data_poll = Body::poll_frame(body, cx);
        let frame_result = ready!(data_poll);

        let frame = match frame_result {
            Some(Ok(frame)) => frame,
            Some(Err(err)) => return Poll::Ready(Some(Err(err))),
            None => return Poll::Ready(None), // end of stream
        };

        // Accept only DATA frames.
        let Ok(data) = frame.into_data() else {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        };

        // Process the new bytes.
        self.append_frame(data);

        // If new events were constructed, emit them.
        if let Some(ev) = self.poll_ready_event() {
            return Poll::Ready(Some(Ok(ev)));
        }

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
