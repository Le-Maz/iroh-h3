use std::time::Instant;

use axum::{body::Body, response::IntoResponse, routing::post};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use iroh::Endpoint;
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;

mod mock_discovery {
    include!("mock_discovery.rs");
}

use mock_discovery::MockDiscovery;
use tokio::task::JoinSet;

const ALPN: &[u8] = b"h3";
const PING: &str = "Ping!";
const PING_COUNT: usize = 100;
const PONG: &str = "Pong!";

async fn ping(body: Body) -> impl IntoResponse {
    let body = body.collect().await.unwrap();
    assert_eq!(body.to_bytes(), PING);
    PONG
}

#[tokio::test]
async fn many_requests() {
    let endpoint_1 = Endpoint::builder().bind().await.unwrap();
    let app = axum::Router::new().route("/ping", post(ping));
    let router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let mut discovery = MockDiscovery::new();
    discovery.add_peer(&endpoint_1);
    let endpoint_2 = Endpoint::builder()
        .discovery(discovery)
        .bind()
        .await
        .unwrap();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/ping", endpoint_1.id());
    let ping_bytes = Bytes::from_static(PING.as_bytes());
    let request = client.post(&uri).body(Full::new(ping_bytes)).unwrap();

    let mut prev = Instant::now();
    for _ in 0..PING_COUNT {
        let mut response = request.clone().send().await.unwrap();
        let response_bytes = response.body_bytes().await.unwrap();
        assert_eq!(response_bytes, PONG);

        let now = Instant::now();
        println!("Request processed in {:?}", now.duration_since(prev));
        prev = now;
    }

    let instant = Instant::now();
    let mut join_set = JoinSet::new();
    for _ in 0..PING_COUNT {
        join_set.spawn(request.clone().send());
    }
    join_set.join_all().await;
    println!("Request burst processed in {:?}", instant.elapsed());

    router.shutdown().await.unwrap();
}

#[tokio::test]
async fn request_burst() {
    let endpoint_1 = Endpoint::builder().bind().await.unwrap();
    let app = axum::Router::new().route("/ping", post(ping));
    let router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let mut discovery = MockDiscovery::new();
    discovery.add_peer(&endpoint_1);
    let endpoint_2 = Endpoint::builder()
        .discovery(discovery)
        .bind()
        .await
        .unwrap();

    let client = IrohH3Client::new(endpoint_2.clone(), ALPN.into());
    let uri = format!("iroh+h3://{}/ping", endpoint_1.id());
    let ping_bytes = Bytes::from_static(PING.as_bytes());
    let request = client.post(&uri).body(Full::new(ping_bytes)).unwrap();
    endpoint_1.online().await;
    endpoint_2.online().await;

    let instant = Instant::now();
    let mut join_set = JoinSet::new();
    for _ in 0..PING_COUNT {
        join_set.spawn(request.clone().send());
    }
    join_set.join_all().await;
    println!("Request burst processed in {:?}", instant.elapsed());
    router.shutdown().await.unwrap();
}
