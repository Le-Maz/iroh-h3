use axum::{extract::State, response::IntoResponse, routing::get};
use example::create_endpoint_pair;
use iroh::EndpointId;
use iroh_h3_axum::{IrohAxum, RemoteId};
use iroh_h3_client::IrohH3Client;

const ALPN: &[u8] = b"h3";
const PONG: &str = "Pong!";

async fn ping(
    RemoteId(remote_id): RemoteId,
    State(allowed_id): State<EndpointId>,
) -> impl IntoResponse {
    assert_eq!(allowed_id, remote_id);
    PONG
}

#[tokio::test]
async fn remote_id() {
    let (endpoint_1, endpoint_2) = create_endpoint_pair().await;

    let app = axum::Router::new()
        .route("/ping", get(ping))
        .with_state(endpoint_2.id());
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/ping", endpoint_1.id());
    let request = client.get(&uri);

    let mut response = request.send().await.unwrap();
    assert_eq!(response.bytes().await.unwrap(), PONG);
}
