use axum::{Json, response::IntoResponse, routing::post};
use example::create_endpoint_pair;
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;

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
    let (endpoint_1, endpoint_2) = create_endpoint_pair().await;

    let app = axum::Router::new().route("/ping", post(ping));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/ping", endpoint_1.id());
    let message = Message {
        message: PING.into(),
    };
    let request = client.post(&uri).json(&message).unwrap();

    let mut response = request.send().await.unwrap();
    assert_eq!(
        response.headers.get("Content-Type").unwrap(),
        "application/json"
    );
    let response_message: Message = response.json().await.unwrap();
    assert_eq!(response_message.message, PONG);
}
