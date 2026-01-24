use bytes::Bytes;
use example::mock_discovery::MockDiscoveryMap;
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;

use axum::{Router, response::IntoResponse, routing::get};

const ALPN: &[u8] = b"iroh+h3";

/// Basic request & headers
#[tokio::test]
async fn basic_get_and_headers() {
    let discovery = MockDiscoveryMap::new();
    let endpoint_1 = discovery.spawn_endpoint().await;
    let endpoint_2 = discovery.spawn_endpoint().await;
    endpoint_1.online().await;
    endpoint_2.online().await;

    /// simple handler returns a static body and sets a custom header
    async fn hello() -> impl IntoResponse {
        (
            axum::response::AppendHeaders([("x-test", "value")]),
            "Hello, World!",
        )
    }

    let app = Router::new().route("/hello", get(hello));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/hello", endpoint_1.id());
    let response = client.get(&uri).send().await.unwrap();

    let header = response.headers.get("x-test").unwrap();
    assert_eq!(header, "value");

    let body = response.bytes().await.unwrap();
    assert_eq!(body, Bytes::from_static(b"Hello, World!"));
}
