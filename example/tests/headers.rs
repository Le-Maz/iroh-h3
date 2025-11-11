use axum::{Json, response::IntoResponse, routing::post};
use http_body_util::Full;
use iroh::Endpoint;
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;

mod mock_discovery {
    include!("mock_discovery.rs");
}

use mock_discovery::MockDiscovery;
use serde::{Deserialize, Serialize};

const ALPN: &[u8] = b"h3";
const PING: &str = "Ping!";
const PONG: &str = "Pong!";

#[derive(Debug, Serialize, Deserialize)]
struct Message {
    message: String,
}

async fn ping(Json(message): Json<Message>) -> impl IntoResponse {
    assert_eq!(message.message, PING);
    Json(Message {
        message: PONG.into(),
    })
}

#[tokio::test]
async fn headers() {
    let endpoint_1 = Endpoint::builder().bind().await.unwrap();
    let app = axum::Router::new().route("/ping", post(ping));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
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
    let message = Message {
        message: PING.into(),
    };
    let json = serde_json::to_string(&message).unwrap();
    let request = client
        .post(&uri)
        .header("Content-Type", "application/json")
        .body(Full::new(json.as_bytes()))
        .unwrap();

    let mut response = request.clone().send().await.unwrap();
    assert_eq!(
        response.headers.get("Content-Type").unwrap(),
        "application/json"
    );
    let response_bytes = response.body_bytes().await.unwrap();
    let response_message: Message = serde_json::from_slice(&response_bytes).unwrap();
    assert_eq!(response_message.message, PONG);
}
