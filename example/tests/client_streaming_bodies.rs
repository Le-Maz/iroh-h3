use std::convert::Infallible;

use bytes::Bytes;
use futures::StreamExt;
use http_body::Frame;
use http_body_util::StreamBody;
use iroh::Endpoint;
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;

use axum::{
    Router,
    body::Body,
    response::IntoResponse,
    routing::{get, post},
};

const ALPN: &[u8] = b"iroh+h3";

/// Streaming responses
#[tokio::test]
async fn streaming_response() {
    let endpoint_1 = Endpoint::bind().await.unwrap();
    let endpoint_2 = Endpoint::bind().await.unwrap();
    endpoint_1.online().await;
    endpoint_2.online().await;

    /// server: stream "Pong!" 10 times
    async fn streaming_ping() -> impl IntoResponse {
        let stream = futures::stream::repeat(Ok::<Bytes, Infallible>(Bytes::from_static(b"Pong!")));
        Body::from_stream(stream.take(10))
    }

    let app = Router::new().route("/streaming-ping", get(streaming_ping));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/streaming-ping", endpoint_1.id());
    let mut response = client.get(&uri).send().await.unwrap();

    let mut stream = response.bytes_stream();
    let mut count = 0usize;
    while let Some(chunk) = stream.next().await.transpose().unwrap() {
        assert_eq!(chunk, b"Pong!"[..]);
        count += 1;
    }
    assert_eq!(count, 10);
}

/// Streaming request body
#[tokio::test]
async fn streaming_request_body() {
    let endpoint_1 = Endpoint::bind().await.unwrap();
    let endpoint_2 = Endpoint::bind().await.unwrap();
    endpoint_1.online().await;
    endpoint_2.online().await;

    const PING: &str = "Ping!";
    const PING_COUNT: usize = 5;
    const PONG: &str = "Pong!";
    const PONG_COUNT: usize = 7;

    async fn streaming_ping(body: Body) -> impl IntoResponse {
        let mut body_stream = body.into_data_stream();
        let mut counter = 0usize;
        while let Some(chunk) = body_stream.next().await.transpose().unwrap() {
            assert_eq!(chunk, PING.as_bytes());
            counter += 1;
        }
        assert_eq!(counter, PING_COUNT);

        let pong_bytes = Bytes::from_static(PONG.as_bytes());
        let ok = Ok::<Bytes, Infallible>(pong_bytes);
        Body::from_stream(futures::stream::repeat(ok).take(PONG_COUNT))
    }

    let app = Router::new().route("/streaming-ping", post(streaming_ping));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/streaming-ping", endpoint_1.id());

    let ping_bytes = Bytes::from_static(PING.as_bytes());
    let frame = || Frame::data(ping_bytes.clone());
    let stream = futures::stream::repeat_with(|| Ok::<_, Infallible>(frame()));
    let body = StreamBody::new(stream.take(PING_COUNT));

    let mut response = client.post(uri).body(body).unwrap().send().await.unwrap();
    let mut resp_stream = response.bytes_stream();
    let mut count = 0usize;
    while let Some(chunk) = resp_stream.next().await.transpose().unwrap() {
        assert_eq!(chunk, PONG.as_bytes());
        count += 1;
    }
    assert_eq!(count, PONG_COUNT);
}
