use iroh::{Endpoint, EndpointId};
use iroh_h3_axum::{IrohAxum, RemoteId};
use iroh_h3_client::IrohH3Client;

use axum::{Router, extract::State, response::IntoResponse, routing::get};

const ALPN: &[u8] = b"iroh+h3";

/// RemoteId extraction
#[tokio::test]
async fn remote_id_extraction() {
    let endpoint_1 = Endpoint::bind().await.unwrap();
    let endpoint_2 = Endpoint::bind().await.unwrap();
    endpoint_1.online().await;
    endpoint_2.online().await;

    async fn handler(
        RemoteId(remote): RemoteId,
        State(expected): State<EndpointId>,
    ) -> impl IntoResponse {
        assert_eq!(remote, expected);
        "ok"
    }

    let app = Router::new()
        .route("/whoami", get(handler))
        .with_state(endpoint_2.id());
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/whoami", endpoint_1.id());

    let response = client.get(&uri).send().await.unwrap();
    assert_eq!(response.bytes().await.unwrap(), b"ok"[..]);
}
