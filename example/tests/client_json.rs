use axum::{Json, Router, response::IntoResponse, routing::post};
use iroh::Endpoint;
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;

use serde::{Deserialize, Serialize};

const ALPN: &[u8] = b"iroh+h3";

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Message {
    message: String,
}

const PING: &str = "Ping!";
const PONG: &str = "Pong!";

#[tokio::test]
async fn json_request_response() {
    let endpoint_1 = Endpoint::bind().await.unwrap();
    let endpoint_2 = Endpoint::bind().await.unwrap();
    endpoint_1.online().await;
    endpoint_2.online().await;

    async fn ping(Json(msg): Json<Message>) -> impl IntoResponse {
        assert_eq!(msg.message, PING);
        Json(Message {
            message: PONG.into(),
        })
    }

    let app = Router::new().route("/ping", post(ping));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/ping", endpoint_1.id());

    let req = client
        .post(&uri)
        .json(&Message {
            message: PING.into(),
        })
        .unwrap();
    let res = req.send().await.unwrap();

    assert_eq!(res.headers.get("Content-Type").unwrap(), "application/json");

    let reply: Message = res.json().await.unwrap();
    assert_eq!(
        reply,
        Message {
            message: PONG.into()
        }
    );
}
