use std::time::Instant;

use example::mock_discovery::MockDiscoveryMap;
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;
// use tokio::task::JoinSet;
use n0_future::task::JoinSet; // unifies wasm/tokio task spawning.

use axum::{Router, routing::post};

const ALPN: &[u8] = b"iroh+h3";

/// Connection reuse / many requests
#[tokio::test]
async fn many_requests_connection_reuse() {
    let discovery = MockDiscoveryMap::new();
    let endpoint_1 = discovery.spawn_endpoint().await;
    let endpoint_2 = discovery.spawn_endpoint().await;
    endpoint_1.online().await;
    endpoint_2.online().await;

    async fn ping() -> &'static str {
        "Pong!"
    }

    let app = Router::new().route("/ping", post(ping));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2.clone(), ALPN.into());
    let uri = format!("iroh+h3://{}/ping", endpoint_1.id());

    for _ in 0..10 {
        let res = client.post(&uri).send().await.unwrap();
        assert_eq!(res.bytes().await.unwrap(), b"Pong!"[..]);
    }

    let instant = Instant::now();
    let mut set = JoinSet::new();
    for _ in 0..50 {
        let request = client.post(&uri).build().unwrap();
        set.spawn(async move {
            let response = request.send().await.unwrap();
            response.bytes().await.unwrap();
        });
    }
    set.join_all().await;
    println!("Burst processed in {:?}", instant.elapsed());
}
