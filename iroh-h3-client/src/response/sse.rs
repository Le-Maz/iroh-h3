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
    pin::Pin,
    task::{Poll, ready},
};

use bytes::Buf;
use futures::Stream;

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
    /// The HTTP/3 response providing the raw SSE byte stream.
    response: Response,

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
            response,
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
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        // Emit any buffered complete event first.
        if let Some(event) = self.advance_line_cursor() {
            return Poll::Ready(Some(Ok(event)));
        }

        // Poll the underlying response body stream.
        let data_result = ready!(self.response.stream.poll_recv_data(cx));
        let frame = match data_result.transpose() {
            Some(Ok(frame)) => frame,
            Some(Err(error)) if !error.is_h3_no_error() => {
                return Poll::Ready(Some(Err(error.into())));
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
        self.append_frame(frame);

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
