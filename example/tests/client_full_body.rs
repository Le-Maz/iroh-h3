use bytes::Bytes;
use http_body_util::BodyExt as _;
use iroh::Endpoint;
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;

use axum::{Router, body::Body, response::IntoResponse, routing::post};

const ALPN: &[u8] = b"iroh+h3";

/// Full-body convenience
#[tokio::test]
async fn full_body_helpers() {
    let endpoint_1 = Endpoint::bind().await.unwrap();
    let endpoint_2 = Endpoint::bind().await.unwrap();
    endpoint_1.online().await;
    endpoint_2.online().await;

    async fn echo_full(body: Body) -> impl IntoResponse {
        let b = body.collect().await.unwrap();
        b.to_bytes()
    }

    let app = Router::new().route("/echo", post(echo_full));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/echo", endpoint_1.id());

    let payload = Bytes::from_static(b"hello-bytes");
    let request = client.post(&uri).bytes(payload.clone()).unwrap();
    let response = request.send().await.unwrap();

    let got = response.bytes().await.unwrap();
    assert_eq!(got, payload);
}
