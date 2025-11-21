use std::convert::Infallible;

use axum::{
    Json, Router,
    body::Body,
    response::IntoResponse,
    routing::{get, post},
};
use bytes::Bytes;
use futures::{StreamExt, stream::repeat_with};
use iroh::Endpoint;
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;
use serde::{Deserialize, Serialize};

const ALPN: &[u8] = b"h3";

// -------------------
// Shared data types
// -------------------
#[derive(Debug, Serialize, Deserialize)]
struct Message {
    sender: String,
    content: String,
}

// -------------------
// Server handlers
// -------------------
async fn broadcast_message(Json(msg): Json<Message>) -> impl IntoResponse {
    println!("Server received: {} says '{}'", msg.sender, msg.content);
    Json(Message {
        sender: "Server".into(),
        content: format!("Echo: {}", msg.content),
    })
}

// Simulate a continuous message stream
async fn message_stream() -> impl IntoResponse {
    let stream = repeat_with(|| {
        let msg = Bytes::from_static(b"Server heartbeat\n");
        Ok::<Bytes, Infallible>(msg)
    });
    Body::from_stream(stream.take(5))
}

// -------------------
// Main
// -------------------
#[tokio::main]
async fn main() {
    // Create two endpoints
    let endpoint_1 = Endpoint::bind().await.unwrap();
    let endpoint_2 = Endpoint::bind().await.unwrap();
    endpoint_1.online().await;
    endpoint_2.online().await;

    // Setup Axum router for endpoint_1
    let app = Router::new()
        .route("/broadcast", post(broadcast_message))
        .route("/heartbeat", get(message_stream));

    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    // -------------------
    // Client sending messages
    // -------------------
    let client = IrohH3Client::new(endpoint_2, ALPN.into());

    // Send a chat message
    let uri = format!("iroh+h3://{}/broadcast", endpoint_1.id());
    let msg = Message {
        sender: "Client".into(),
        content: "Hello from endpoint 2!".into(),
    };

    let response = client.post(&uri).json(&msg).unwrap().send().await.unwrap();
    let reply: Message = response.json().await.unwrap();
    println!("Received reply: {} says '{}'", reply.sender, reply.content);

    // Subscribe to heartbeat stream
    let uri = format!("iroh+h3://{}/heartbeat", endpoint_1.id());
    let response = client.get(&uri).send().await.unwrap();
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await.transpose().unwrap() {
        println!("Heartbeat: {}", String::from_utf8_lossy(&chunk));
    }
}
