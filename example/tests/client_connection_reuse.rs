use std::time::Instant;

use iroh::Endpoint;
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;
use tokio::task::JoinSet;

use axum::{Router, routing::post};

const ALPN: &[u8] = b"iroh+h3";

/// Connection reuse / many requests
#[tokio::test]
async fn many_requests_connection_reuse() {
    let endpoint_1 = Endpoint::bind().await.unwrap();
    let endpoint_2 = Endpoint::bind().await.unwrap();
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
        let mut res = client.post(&uri).send().await.unwrap();
        assert_eq!(res.bytes().await.unwrap(), b"Pong!"[..]);
    }

    let request = client.post(&uri).build().unwrap();
    let instant = Instant::now();
    let mut set = JoinSet::new();
    for _ in 0..50 {
        let req_clone = request.clone();
        set.spawn(async move {
            let mut r = req_clone.send().await.unwrap();
            r.bytes().await.unwrap();
        });
    }
    set.join_all().await;
    println!("Burst processed in {:?}", instant.elapsed());
}
