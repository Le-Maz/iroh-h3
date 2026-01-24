use iroh::{Endpoint, EndpointId};
use iroh_h3_client::IrohH3Client;

const ALPN: &[u8] = b"iroh+h3";

/// Error handling
#[tokio::test]
async fn error_handling_unresolvable_peer() {
    let endpoint = Endpoint::bind().await.unwrap();
    endpoint.online().await;

    let client = IrohH3Client::new(endpoint, ALPN.into());

    let fake_id = EndpointId::from_bytes(b"fsdgh righrfdruigrfiuyrghsidugjm").unwrap();
    let uri = format!("iroh+h3://{}/ping", fake_id);

    let res = client.get(&uri).send().await;
    assert!(
        res.is_err(),
        "expected error when sending to an unresolvable peer, got Ok"
    );
}
