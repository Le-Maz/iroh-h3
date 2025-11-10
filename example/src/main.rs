use std::convert::Infallible;

use axum::{body::Body, response::IntoResponse, routing::get};
use bytes::Bytes;
use futures::{StreamExt, stream::repeat};
use http::Request;
use http_body_util::Empty;
use iroh::{Endpoint, discovery::dns::DnsDiscovery};
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;

const ALPN: &[u8] = b"h3";
const PONG: &str = "Pong!";

async fn streaming_ping() -> impl IntoResponse {
    Body::from_stream(repeat(Ok::<Bytes, Infallible>(Bytes::from_static(
        PONG.as_bytes(),
    ))))
}

#[tokio::main]
async fn main() {
    let endpoint_1 = Endpoint::builder()
        .discovery(DnsDiscovery::n0_dns())
        .bind()
        .await
        .unwrap();
    endpoint_1.online().await;
    let app = axum::Router::new().route("/streaming-ping", get(streaming_ping));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let endpoint_2 = Endpoint::builder()
        .discovery(DnsDiscovery::n0_dns())
        .bind()
        .await
        .unwrap();
    endpoint_2.online().await;
    let client = IrohH3Client::new(endpoint_2, ALPN.into());

    let request = Request::builder()
        .uri(format!("https://{}/streaming-ping", endpoint_1.id()))
        .body(Empty::<Bytes>::new())
        .unwrap();

    let mut response = client.send(request).await.unwrap();
    println!("Sent PING!");
    let mut response_body_stream = response.body_stream();
    while let Some(data) = response_body_stream.next().await.transpose().unwrap() {
        println!("Received a frame with {} bytes", data.len());
        assert_eq!(PONG.as_bytes(), data);
    }
}
