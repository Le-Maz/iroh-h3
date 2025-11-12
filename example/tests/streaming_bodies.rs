use std::convert::Infallible;

use axum::{body::Body, response::IntoResponse, routing::post};
use bytes::Bytes;
use example::create_endpoint_pair;
use futures::{
    StreamExt,
    stream::{repeat, repeat_with},
};
use http_body::Frame;
use http_body_util::StreamBody;
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;

const ALPN: &[u8] = b"h3";
const PING: &str = "Ping!";
const PING_COUNT: usize = 5;
const PONG: &str = "Pong!";
const PONG_COUNT: usize = 7;

async fn streaming_ping(body: Body) -> impl IntoResponse {
    let mut body_stream = body.into_data_stream();
    let mut counter = 0;
    while let Some(chunk) = body_stream.next().await.transpose().unwrap() {
        println!("Server received data");
        assert_eq!(chunk, PING.as_bytes());
        counter += 1;
    }
    assert_eq!(counter, PING_COUNT);
    let pong_bytes = Bytes::from_static(PONG.as_bytes());
    let ok_pong = Ok::<Bytes, Infallible>(pong_bytes);
    let response_stream = repeat(ok_pong);
    Body::from_stream(response_stream.take(PONG_COUNT))
}

#[tokio::test]
async fn streaming_bodies() {
    let (endpoint_1, endpoint_2) = create_endpoint_pair().await;

    let app = axum::Router::new().route("/streaming-ping", post(streaming_ping));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/streaming-ping", endpoint_1.id());

    let ping_bytes = Bytes::from_static(PING.as_bytes());
    let frame = || Frame::data(ping_bytes.clone());
    let stream = repeat_with(|| Ok::<_, Infallible>(frame()));
    let body = StreamBody::new(stream.take(PING_COUNT));
    let mut response = client.post(uri).body(body).unwrap().send().await.unwrap();
    let mut body_stream = response.bytes_stream();
    let mut counter = 0;
    while let Some(chunk) = body_stream.next().await.transpose().unwrap() {
        println!("Client received data");
        assert_eq!(chunk, PONG.as_bytes());
        counter += 1;
    }
    assert_eq!(counter, PONG_COUNT);
}
