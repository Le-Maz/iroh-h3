//! HTTP/3 datagrams over iroh connections, without relays or discovery (direct local addresses).

use std::time::Duration;

use bytes::Bytes;
use h3_datagram::datagram_handler::HandleDatagramsExt;
use iroh::{Endpoint, EndpointAddr, endpoint::presets};
use tokio::time::timeout;

const ALPN: &[u8] = b"h3";

/// Binds two endpoints and connects the second to the first one.
async fn connect() -> (iroh::endpoint::Connection, iroh::endpoint::Connection) {
    let server = Endpoint::builder(presets::Minimal)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .unwrap();
    let client = Endpoint::builder(presets::Minimal).bind().await.unwrap();

    let mut addr = EndpointAddr::new(server.id());
    for socket in server.bound_sockets() {
        addr = addr.with_ip_addr((std::net::Ipv4Addr::LOCALHOST, socket.port()).into());
    }

    let accept = async {
        let incoming = server.accept().await.unwrap();
        incoming.accept().unwrap().await.unwrap()
    };
    let (server_conn, client_conn) =
        tokio::join!(accept, async { client.connect(addr, ALPN).await.unwrap() });

    // Keep the endpoints alive as long as the connections.
    tokio::spawn(async move {
        let _keep = (server, client);
        std::future::pending::<()>().await;
    });

    (server_conn, client_conn)
}

/// A datagram of a request stream is delivered to the peer, in both directions.
#[tokio::test]
async fn datagrams_in_both_directions() {
    timeout(Duration::from_secs(20), async {
        let (server_conn, client_conn) = connect().await;

        let mut server = h3::server::builder()
            .enable_datagram(true)
            .build(iroh_h3::Connection::new(server_conn))
            .await
            .unwrap();
        let (mut client, mut send_request) = h3::client::builder()
            .enable_datagram(true)
            .build(iroh_h3::Connection::new(client_conn))
            .await
            .unwrap();

        let mut server_reader = server.get_datagram_reader();
        let mut client_reader = client.get_datagram_reader();

        // The first request of the client is on the first client-initiated bidirectional stream.
        let stream_id = h3::quic::StreamId::try_from(0).unwrap();
        let mut client_sender = client.get_datagram_sender(stream_id);

        // The server also sends its datagrams, once it knows the request.
        let serve = tokio::spawn(async move {
            let resolver = server.accept().await.unwrap().unwrap();
            let (_request, stream) = resolver.resolve_request().await.unwrap();
            let mut sender = server.get_datagram_sender(stream.send_id());

            // Sending is possible as soon as the SETTINGS of the peer have arrived.
            loop {
                match sender.send_datagram(Bytes::from_static(b"from the server")) {
                    Ok(()) => break,
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
            // Keep the connection (and the stream) open until the end of the test.
            let _stream = stream;
            let _ = server.accept().await;
        });

        // Drive the connection of the client.
        let driver = tokio::spawn(async move {
            let _ = client.wait_idle().await;
        });

        // The request, that the datagrams belong to.
        let request = http::Request::get("https://iroh.test/").body(()).unwrap();
        let mut request_stream = send_request.send_request(request).await.unwrap();
        assert_eq!(request_stream.id(), stream_id);
        // Sends the request.
        request_stream.finish().await.unwrap();

        loop {
            match client_sender.send_datagram(Bytes::from_static(b"from the client")) {
                Ok(()) => break,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }

        let datagram = server_reader.read_datagram().await.unwrap();
        assert_eq!(datagram.stream_id(), stream_id);
        assert_eq!(datagram.payload().as_ref(), b"from the client");

        let datagram = client_reader.read_datagram().await.unwrap();
        assert_eq!(datagram.stream_id(), stream_id);
        assert_eq!(datagram.payload().as_ref(), b"from the server");

        driver.abort();
        serve.abort();
    })
    .await
    .expect("timeout");
}
