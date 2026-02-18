//! # iroh-h3: HTTP/3 support over iroh P2P connections
//!
//! `iroh-h3` provides low-level integration for running HTTP/3 over
//! [`iroh`](https://docs.rs/iroh) peer-to-peer QUIC connections.
//! It implements the traits required by the `h3` crate on top of `iroh`
//! connections and streams. This crate is intended for internal use in building
//! HTTP/3 over P2P layers.
//!
//! # License
//!
//! This crate is MIT licensed. Portions of the code are derived from
//! [`hyperium/h3`](https://github.com/hyperium/h3) and are reproduced under
//! the original MIT license terms.

#![deny(missing_docs)]

use std::{
    convert::TryInto,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{self, Poll},
};

use bytes::{Buf, Bytes};
use futures::{
    Stream, StreamExt, ready,
    stream::{self},
};
use h3::{
    error::Code,
    quic::{self, ConnectionErrorIncoming, StreamErrorIncoming, StreamId, WriteBuf},
};
pub use iroh::endpoint::{AcceptBi, AcceptUni, Endpoint, OpenBi, OpenUni, VarInt};
use iroh::endpoint::{ConnectionError, ReadError, WriteError};
use tokio_util::sync::ReusableBoxFuture;

/// BoxStream type alias with `Sync` and `Send` requirements.
type BoxStreamSync<'a, T> = Pin<Box<dyn Stream<Item = T> + Sync + Send + 'a>>;

/// A wrapper around an [`iroh::endpoint::Connection`] that implements the
/// [`h3::quic::Connection`] trait for use in HTTP/3 over QUIC.
///
/// This struct manages incoming and outgoing unidirectional and bidirectional
/// streams and handles conversions between `iroh` and `h3` errors.
pub struct Connection {
    conn: iroh::endpoint::Connection,
    incoming_bi: BoxStreamSync<'static, <AcceptBi<'static> as Future>::Output>,
    opening_bi: Option<BoxStreamSync<'static, <OpenBi<'static> as Future>::Output>>,
    incoming_uni: BoxStreamSync<'static, <AcceptUni<'static> as Future>::Output>,
    opening_uni: Option<BoxStreamSync<'static, <OpenUni<'static> as Future>::Output>>,
}

impl Connection {
    /// Creates a new [`Connection`] from an existing [`iroh::endpoint::Connection`].
    ///
    /// This sets up async streams for accepting incoming unidirectional and
    /// bidirectional QUIC streams.
    ///
    /// # Arguments
    ///
    /// * `conn` - The underlying `iroh` connection to wrap.
    pub fn new(conn: iroh::endpoint::Connection) -> Self {
        Self {
            conn: conn.clone(),
            incoming_bi: Box::pin(stream::unfold(conn.clone(), |conn| async {
                Some((conn.accept_bi().await, conn))
            })),
            opening_bi: None,
            incoming_uni: Box::pin(stream::unfold(conn.clone(), |conn| async {
                Some((conn.accept_uni().await, conn))
            })),
            opening_uni: None,
        }
    }
}

impl<B> quic::Connection<B> for Connection
where
    B: Buf,
{
    type RecvStream = RecvStream;
    type OpenStreams = OpenStreams;

    /// Polls for an incoming bidirectional stream (accepts a stream).
    ///
    /// Returns a pair of [`SendStream`] and [`RecvStream`] wrapped in
    /// [`BidiStream`].
    fn poll_accept_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::BidiStream, ConnectionErrorIncoming>> {
        let (send, recv) = ready!(self.incoming_bi.poll_next_unpin(cx))
            .expect("self.incoming_bi BoxStream never returns None")
            .map_err(convert_connection_error)?;
        Poll::Ready(Ok(Self::BidiStream {
            send: Self::SendStream::new(send),
            recv: Self::RecvStream::new(recv),
        }))
    }

    /// Polls for an incoming unidirectional receive stream.
    ///
    /// Returns a [`RecvStream`] once available.
    fn poll_accept_recv(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::RecvStream, ConnectionErrorIncoming>> {
        let recv = ready!(self.incoming_uni.poll_next_unpin(cx))
            .expect("self.incoming_uni BoxStream never returns None")
            .map_err(convert_connection_error)?;
        Poll::Ready(Ok(Self::RecvStream::new(recv)))
    }

    /// Returns a new [`OpenStreams`] handle for opening outgoing streams.
    fn opener(&self) -> Self::OpenStreams {
        OpenStreams {
            conn: self.conn.clone(),
            opening_bi: None,
            opening_uni: None,
        }
    }
}

/// Converts an [`iroh::endpoint::ConnectionError`] to an [`h3::quic::ConnectionErrorIncoming`].
fn convert_connection_error(e: ConnectionError) -> h3::quic::ConnectionErrorIncoming {
    match e {
        ConnectionError::ApplicationClosed(application_close) => {
            ConnectionErrorIncoming::ApplicationClose {
                error_code: application_close.error_code.into(),
            }
        }
        ConnectionError::TimedOut => ConnectionErrorIncoming::Timeout,
        error @ ConnectionError::VersionMismatch
        | error @ ConnectionError::Reset
        | error @ ConnectionError::LocallyClosed
        | error @ ConnectionError::CidsExhausted
        | error @ ConnectionError::TransportError(_)
        | error @ ConnectionError::ConnectionClosed(_) => {
            ConnectionErrorIncoming::Undefined(Arc::new(error))
        }
    }
}

impl<B> quic::OpenStreams<B> for Connection
where
    B: Buf,
{
    type SendStream = SendStream<B>;
    type BidiStream = BidiStream<B>;

    /// Attempts to open a new bidirectional stream for sending and receiving.
    ///
    /// Returns a [`BidiStream`] once ready, or a [`StreamErrorIncoming`] on failure.
    fn poll_open_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        let bi = self.opening_bi.get_or_insert_with(|| {
            Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((conn.open_bi().await, conn))
            }))
        });
        let (send, recv) = ready!(bi.poll_next_unpin(cx))
            .expect("BoxStream does not return None")
            .map_err(|e| StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_connection_error(e),
            })?;
        Poll::Ready(Ok(Self::BidiStream {
            send: Self::SendStream::new(send),
            recv: RecvStream::new(recv),
        }))
    }

    /// Attempts to open a new unidirectional send stream.
    ///
    /// Returns a [`SendStream`] once ready.
    fn poll_open_send(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::SendStream, StreamErrorIncoming>> {
        let uni = self.opening_uni.get_or_insert_with(|| {
            Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((conn.open_uni().await, conn))
            }))
        });

        let send = ready!(uni.poll_next_unpin(cx))
            .expect("BoxStream does not return None")
            .map_err(|e| StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_connection_error(e),
            })?;
        Poll::Ready(Ok(Self::SendStream::new(send)))
    }

    /// Closes the QUIC connection with the provided application error code and reason.
    fn close(&mut self, code: Code, reason: &[u8]) {
        self.conn.close(
            VarInt::from_u64(code.value()).expect("error code VarInt"),
            reason,
        );
    }
}

/// A handle for opening outgoing QUIC streams.
///
/// Implements [`h3::quic::OpenStreams`] for use with HTTP/3.
pub struct OpenStreams {
    conn: iroh::endpoint::Connection,
    opening_bi: Option<BoxStreamSync<'static, <OpenBi<'static> as Future>::Output>>,
    opening_uni: Option<BoxStreamSync<'static, <OpenUni<'static> as Future>::Output>>,
}

impl<B> quic::OpenStreams<B> for OpenStreams
where
    B: Buf,
{
    type SendStream = SendStream<B>;
    type BidiStream = BidiStream<B>;

    /// Polls for opening a new bidirectional stream.
    ///
    /// Returns a [`BidiStream`] on success.
    fn poll_open_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        let bi = self.opening_bi.get_or_insert_with(|| {
            Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((conn.open_bi().await, conn))
            }))
        });

        let (send, recv) = ready!(bi.poll_next_unpin(cx))
            .expect("BoxStream does not return None")
            .map_err(|e| StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_connection_error(e),
            })?;
        Poll::Ready(Ok(Self::BidiStream {
            send: Self::SendStream::new(send),
            recv: RecvStream::new(recv),
        }))
    }

    /// Polls for opening a new unidirectional send stream.
    ///
    /// Returns a [`SendStream`] on success.
    fn poll_open_send(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::SendStream, StreamErrorIncoming>> {
        let uni = self.opening_uni.get_or_insert_with(|| {
            Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((conn.open_uni().await, conn))
            }))
        });

        let send = ready!(uni.poll_next_unpin(cx))
            .expect("BoxStream does not return None")
            .map_err(|e| StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_connection_error(e),
            })?;
        Poll::Ready(Ok(Self::SendStream::new(send)))
    }

    /// Closes the underlying connection with the given error code and reason.
    fn close(&mut self, code: Code, reason: &[u8]) {
        self.conn.close(
            VarInt::from_u64(code.value()).expect("error code VarInt"),
            reason,
        );
    }
}

/// Implements [`Clone`] for [`OpenStreams`].
impl Clone for OpenStreams {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
            opening_bi: None,
            opening_uni: None,
        }
    }
}

/// A bidirectional QUIC stream that contains both send and receive halves.
///
/// This struct implements both [`h3::quic::BidiStream`], [`h3::quic::RecvStream`],
/// and [`h3::quic::SendStream`] traits, allowing it to be split or used directly.
pub struct BidiStream<B>
where
    B: Buf,
{
    send: SendStream<B>,
    recv: RecvStream,
}

impl<B> quic::BidiStream<B> for BidiStream<B>
where
    B: Buf,
{
    type SendStream = SendStream<B>;
    type RecvStream = RecvStream;

    /// Splits the bidirectional stream into its send and receive halves.
    ///
    /// # Returns
    /// A tuple of `(SendStream, RecvStream)`.
    fn split(self) -> (Self::SendStream, Self::RecvStream) {
        (self.send, self.recv)
    }
}

impl<B: Buf> quic::RecvStream for BidiStream<B> {
    type Buf = Bytes;

    /// Polls for incoming data on the receive side of the stream.
    ///
    /// Returns `Poll::Ready(Ok(Some(Bytes)))` when data is available,
    /// `Poll::Ready(Ok(None))` when the stream is finished,
    /// or `Poll::Ready(Err(StreamErrorIncoming))` on error.
    fn poll_data(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<Self::Buf>, StreamErrorIncoming>> {
        self.recv.poll_data(cx)
    }

    /// Informs the peer that the receiver is no longer interested in this stream.
    fn stop_sending(&mut self, error_code: u64) {
        self.recv.stop_sending(error_code)
    }

    /// Returns the QUIC stream ID for this receiving stream.
    fn recv_id(&self) -> StreamId {
        self.recv.recv_id()
    }
}

impl<B> quic::SendStream<B> for BidiStream<B>
where
    B: Buf,
{
    /// Polls for readiness to send data on the stream.
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        self.send.poll_ready(cx)
    }

    /// Polls for completion of the stream’s send side (finishing transmission).
    fn poll_finish(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        self.send.poll_finish(cx)
    }

    /// Resets the send side of the stream with an error code.
    fn reset(&mut self, reset_code: u64) {
        self.send.reset(reset_code)
    }

    /// Queues a buffer of data to be sent on the stream.
    fn send_data<D: Into<WriteBuf<B>>>(&mut self, data: D) -> Result<(), StreamErrorIncoming> {
        self.send.send_data(data)
    }

    /// Returns the QUIC stream ID for this sending stream.
    fn send_id(&self) -> StreamId {
        self.send.send_id()
    }
}

impl<B> quic::SendStreamUnframed<B> for BidiStream<B>
where
    B: Buf,
{
    /// Polls to send raw unframed data from the provided buffer.
    ///
    /// This variant writes directly from the buffer without framing.
    fn poll_send<D: Buf>(
        &mut self,
        cx: &mut task::Context<'_>,
        buf: &mut D,
    ) -> Poll<Result<usize, StreamErrorIncoming>> {
        self.send.poll_send(cx, buf)
    }
}

/// A receiving QUIC stream that reads ordered chunks of data.
///
/// Internally wraps an [`iroh::endpoint::RecvStream`] and manages
/// reusable futures for efficient reading.
pub struct RecvStream {
    stream: Option<iroh::endpoint::RecvStream>,
    read_chunk_fut: ReadChunkFuture,
}

/// Type alias for a reusable boxed future that reads the next chunk.
type ReadChunkFuture = ReusableBoxFuture<
    'static,
    (
        iroh::endpoint::RecvStream,
        Result<Option<iroh::endpoint::Chunk>, iroh::endpoint::ReadError>,
    ),
>;

impl RecvStream {
    /// Creates a new [`RecvStream`] from an [`iroh::endpoint::RecvStream`].
    fn new(stream: iroh::endpoint::RecvStream) -> Self {
        Self {
            stream: Some(stream),
            read_chunk_fut: ReusableBoxFuture::new(async { unreachable!() }),
        }
    }
}

impl quic::RecvStream for RecvStream {
    type Buf = Bytes;

    /// Polls for the next chunk of received data.
    ///
    /// Returns:
    /// * `Poll::Ready(Ok(Some(Bytes)))` — when data is available.
    /// * `Poll::Ready(Ok(None))` — when the stream has finished.
    /// * `Poll::Ready(Err(StreamErrorIncoming))` — when an error occurs.
    fn poll_data(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<Self::Buf>, StreamErrorIncoming>> {
        if let Some(mut stream) = self.stream.take() {
            self.read_chunk_fut.set(async move {
                let chunk = stream.read_chunk(usize::MAX).await;
                (stream, chunk)
            })
        };

        let (stream, chunk) = ready!(self.read_chunk_fut.poll(cx));
        self.stream = Some(stream);
        Poll::Ready(Ok(chunk
            .map_err(convert_read_error_to_stream_error)?
            .map(|c| c.bytes)))
    }

    /// Cancels further reception on this stream with the given error code.
    fn stop_sending(&mut self, error_code: u64) {
        self.stream
            .as_mut()
            .unwrap()
            .stop(VarInt::from_u64(error_code).expect("invalid error_code"))
            .ok();
    }

    /// Returns the QUIC stream ID associated with this receive stream.
    fn recv_id(&self) -> StreamId {
        let num: u64 = self.stream.as_ref().unwrap().id().into();
        num.try_into().expect("invalid stream id")
    }
}

/// Converts an [`iroh::endpoint::ReadError`] into an [`h3::quic::StreamErrorIncoming`].
fn convert_read_error_to_stream_error(error: ReadError) -> StreamErrorIncoming {
    match error {
        ReadError::Reset(var_int) => StreamErrorIncoming::StreamTerminated {
            error_code: var_int.into_inner(),
        },
        ReadError::ConnectionLost(connection_error) => {
            StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_connection_error(connection_error),
            }
        }
        error @ ReadError::ClosedStream => StreamErrorIncoming::Unknown(Box::new(error)),
        error @ ReadError::ZeroRttRejected => StreamErrorIncoming::Unknown(Box::new(error)),
    }
}

/// Converts an [`iroh::endpoint::WriteError`] into an [`h3::quic::StreamErrorIncoming`].
fn convert_write_error_to_stream_error(error: WriteError) -> StreamErrorIncoming {
    match error {
        WriteError::Stopped(var_int) => StreamErrorIncoming::StreamTerminated {
            error_code: var_int.into_inner(),
        },
        WriteError::ConnectionLost(connection_error) => {
            StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_connection_error(connection_error),
            }
        }
        error @ WriteError::ClosedStream | error @ WriteError::ZeroRttRejected => {
            StreamErrorIncoming::Unknown(Box::new(error))
        }
    }
}

/// A sending QUIC stream that transmits buffered data.
///
/// This struct wraps an [`iroh::endpoint::SendStream`] and implements the
/// [`h3::quic::SendStream`] and [`h3::quic::SendStreamUnframed`] traits.
pub struct SendStream<B: Buf> {
    stream: iroh::endpoint::SendStream,
    writing: Option<WriteBuf<B>>,
}

impl<B> SendStream<B>
where
    B: Buf,
{
    /// Creates a new [`SendStream`] from an [`iroh::endpoint::SendStream`].
    fn new(stream: iroh::endpoint::SendStream) -> SendStream<B> {
        Self {
            stream,
            writing: None,
        }
    }
}

impl<B> quic::SendStream<B> for SendStream<B>
where
    B: Buf,
{
    /// Polls to check if the stream is ready to send more data.
    ///
    /// If data is pending in `self.writing`, it is written until complete.
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        if let Some(ref mut data) = self.writing {
            while data.has_remaining() {
                let stream = Pin::new(&mut self.stream);
                let written = ready!(stream.poll_write(cx, data.chunk()))
                    .map_err(convert_write_error_to_stream_error)?;
                data.advance(written);
            }
        }
        self.writing = None;
        Poll::Ready(Ok(()))
    }

    /// Finishes sending data on this stream and closes it gracefully.
    fn poll_finish(
        &mut self,
        _cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), StreamErrorIncoming>> {
        Poll::Ready(
            self.stream
                .finish()
                .map_err(|e| StreamErrorIncoming::Unknown(Box::new(e))),
        )
    }

    /// Resets the stream with the provided error code, immediately terminating it.
    fn reset(&mut self, reset_code: u64) {
        let _ = self
            .stream
            .reset(VarInt::from_u64(reset_code).unwrap_or(VarInt::MAX));
    }

    /// Queues data to be sent in the next `poll_ready` call.
    ///
    /// Returns an error if called while another write is still in progress.
    fn send_data<D: Into<WriteBuf<B>>>(&mut self, data: D) -> Result<(), StreamErrorIncoming> {
        if self.writing.is_some() {
            return Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::InternalError(
                    "internal error in the http stack".to_string(),
                ),
            });
        }
        self.writing = Some(data.into());
        Ok(())
    }

    /// Returns the QUIC stream ID for this sending stream.
    fn send_id(&self) -> StreamId {
        let num: u64 = self.stream.id().into();
        num.try_into().expect("invalid stream id")
    }
}

impl<B> quic::SendStreamUnframed<B> for SendStream<B>
where
    B: Buf,
{
    /// Polls to send unframed raw data directly from the provided buffer.
    ///
    /// Returns the number of bytes written on success.
    fn poll_send<D: Buf>(
        &mut self,
        cx: &mut task::Context<'_>,
        buf: &mut D,
    ) -> Poll<Result<usize, StreamErrorIncoming>> {
        if self.writing.is_some() {
            panic!("poll_send called while send stream is not ready");
        }

        let s = Pin::new(&mut self.stream);

        let res = ready!(s.poll_write(cx, buf.chunk()));
        match res {
            Ok(written) => {
                buf.advance(written);
                Poll::Ready(Ok(written))
            }
            Err(err) => Poll::Ready(Err(convert_write_error_to_stream_error(err))),
        }
    }
}
