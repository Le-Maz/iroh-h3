use std::convert::Infallible;

use axum::{
    Router,
    response::{IntoResponse, Sse, sse::Event},
    routing::get,
};
use example::mock_discovery::MockDiscoveryMap;
use futures::{StreamExt, stream::repeat};
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;
use wasm_bindgen_test::wasm_bindgen_test;
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const ALPN: &[u8] = b"iroh+h3";

/// Server-Sent Events
#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn sse_stream() {
    let discovery = MockDiscoveryMap::new();
    let endpoint_1 = discovery.spawn_endpoint().await;
    let endpoint_2 = discovery.spawn_endpoint().await;
    endpoint_1.online().await;
    endpoint_2.online().await;

    /// simple handler returns a static body and sets a custom header
    async fn hello() -> impl IntoResponse {
        let event = Event::default().data("some data");
        let event_result = Ok::<_, Infallible>(event);
        let stream = repeat(event_result);
        Sse::new(stream.take(10))
    }

    let app = Router::new().route("/hello", get(hello));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/hello", endpoint_1.id());
    let response = client.get(&uri).send().await.unwrap();

    let mut sse_stream = response.sse_stream();
    let mut counter = 0;
    while let Some(event) = sse_stream.next().await.transpose().unwrap() {
        counter += 1;
        assert_eq!(event.data(), "some data");
    }
    assert_eq!(counter, 10);
}

#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn sse_stream_edge_cases() {
    use futures::stream::{self, StreamExt};

    let discovery = MockDiscoveryMap::new();
    let endpoint_1 = discovery.spawn_endpoint().await;
    let endpoint_2 = discovery.spawn_endpoint().await;
    endpoint_1.online().await;
    endpoint_2.online().await;

    async fn edge_case_handler() -> impl IntoResponse {
        let events = vec![
            Event::default().data("simple"),
            Event::default().data("line1\nline2"),
            Event::default().data(""), // empty data line
            Event::default().data("payload").id("42").event("custom"),
        ];

        let stream = stream::iter(events.into_iter().map(Ok::<_, Infallible>));
        Sse::new(stream)
    }

    let app = Router::new().route("/sse", get(edge_case_handler));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/sse", endpoint_1.id());
    let response = client.get(&uri).send().await.unwrap();

    let mut sse_stream = response.sse_stream();
    let mut events = Vec::new();
    while let Some(event) = sse_stream.next().await.transpose().unwrap() {
        events.push(event);
    }

    assert_eq!(events.len(), 4);
    assert_eq!(events[0].data(), "simple");
    assert_eq!(events[1].data(), "line1\nline2");
    assert_eq!(events[2].data(), "");
    assert_eq!(events[3].id(), Some("42"));
    assert_eq!(events[3].event(), Some("custom"));
    assert_eq!(events[3].data(), "payload");
}
