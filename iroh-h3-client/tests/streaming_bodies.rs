use std::convert::Infallible;

use axum::{
    Router,
    body::Body,
    response::IntoResponse,
    routing::{get, post},
};
use bytes::Bytes;
use example::mock_discovery::MockAddressLookupMap;
use futures::StreamExt;
use http_body::Frame;
use http_body_util::{StreamBody, combinators::BoxBody};
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;
use wasm_bindgen_test::wasm_bindgen_test;
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const ALPN: &[u8] = b"iroh+h3";

/// Streaming responses
#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn streaming_response() {
    let discovery = MockAddressLookupMap::new();
    let endpoint_1 = discovery.spawn_endpoint().await;
    let endpoint_2 = discovery.spawn_endpoint().await;
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
    let response = client.get(&uri).send().await.unwrap();

    let mut stream = response.bytes_stream();
    let mut count = 0usize;
    while let Some(chunk) = stream.next().await.transpose().unwrap() {
        assert_eq!(chunk, b"Pong!"[..]);
        count += 1;
    }
    assert_eq!(count, 10);
}

/// Streaming request body
#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn streaming_request_body() {
    let discovery = MockAddressLookupMap::new();
    let endpoint_1 = discovery.spawn_endpoint().await;
    let endpoint_2 = discovery.spawn_endpoint().await;
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
    let frame = move || Frame::data(ping_bytes.clone());
    let stream = futures::stream::repeat_with(move || Ok::<_, Infallible>(frame()));
    let body = BoxBody::new(StreamBody::new(stream.take(PING_COUNT))).into();

    let response = client.post(uri).body(body).unwrap().send().await.unwrap();
    let mut resp_stream = response.bytes_stream();
    let mut count = 0usize;
    while let Some(chunk) = resp_stream.next().await.transpose().unwrap() {
        assert_eq!(chunk, PONG.as_bytes());
        count += 1;
    }
    assert_eq!(count, PONG_COUNT);
}
