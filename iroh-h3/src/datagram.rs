//! Support for the [`h3-datagram`](h3_datagram) crate: HTTP/3 datagrams
//! ([RFC 9297](https://www.rfc-editor.org/rfc/rfc9297)) over iroh connections.
//!
//! This implements [`DatagramConnectionExt`] for [`Connection`], which is what extensions like
//! WebTransport (`h3-webtransport`) need. The datagrams are the QUIC datagrams of the iroh
//! connection.

use std::{
    future::Future,
    task::{Poll, ready},
};

use bytes::{Buf, Bytes};
use futures::{StreamExt, stream};
use h3_datagram::{
    ConnectionErrorIncoming,
    datagram::EncodedDatagram,
    quic_traits::{DatagramConnectionExt, RecvDatagram, SendDatagram, SendDatagramErrorIncoming},
};
use iroh::endpoint::{ReadDatagram, SendDatagramError};

use crate::{BoxStreamSync, Connection, convert_connection_error};

/// Sends datagrams over an iroh connection.
pub struct SendDatagramHandler {
    conn: iroh::endpoint::Connection,
}

impl<B: Buf> SendDatagram<B> for SendDatagramHandler {
    fn send_datagram<T: Into<EncodedDatagram<B>>>(
        &mut self,
        data: T,
    ) -> Result<(), SendDatagramErrorIncoming> {
        let mut data: EncodedDatagram<B> = data.into();
        self.conn
            .send_datagram(data.copy_to_bytes(data.remaining()))
            .map_err(convert_send_datagram_error)
    }
}

/// Receives datagrams from an iroh connection.
pub struct RecvDatagramHandler {
    datagrams: BoxStreamSync<'static, <ReadDatagram<'static> as Future>::Output>,
}

impl RecvDatagram for RecvDatagramHandler {
    type Buffer = Bytes;

    fn poll_incoming_datagram(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<Self::Buffer, ConnectionErrorIncoming>> {
        Poll::Ready(
            ready!(self.datagrams.poll_next_unpin(cx))
                .expect("self.datagrams never returns None")
                .map_err(convert_connection_error),
        )
    }
}

impl<B: Buf> DatagramConnectionExt<B> for Connection {
    type SendDatagramHandler = SendDatagramHandler;
    type RecvDatagramHandler = RecvDatagramHandler;

    fn send_datagram_handler(&self) -> Self::SendDatagramHandler {
        SendDatagramHandler {
            conn: self.conn.clone(),
        }
    }

    fn recv_datagram_handler(&self) -> Self::RecvDatagramHandler {
        RecvDatagramHandler {
            datagrams: Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((conn.read_datagram().await, conn))
            })),
        }
    }
}

/// Converts a [`SendDatagramError`] to a [`SendDatagramErrorIncoming`].
fn convert_send_datagram_error(error: SendDatagramError) -> SendDatagramErrorIncoming {
    match error {
        SendDatagramError::UnsupportedByPeer | SendDatagramError::Disabled => {
            SendDatagramErrorIncoming::NotAvailable
        }
        SendDatagramError::TooLarge => SendDatagramErrorIncoming::TooLarge,
        SendDatagramError::ConnectionLost(error) => {
            SendDatagramErrorIncoming::ConnectionError(convert_connection_error(error))
        }
    }
}
