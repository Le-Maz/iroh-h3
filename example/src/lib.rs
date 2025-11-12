use std::{
    collections::{BTreeSet, HashMap},
    net::SocketAddr,
};

use futures_lite::stream::once;
use iroh::{
    Endpoint, EndpointId,
    discovery::{Discovery, DiscoveryError, DiscoveryItem, EndpointInfo},
};

type BoxStream<T> = futures_lite::stream::Boxed<T>;

#[derive(Debug, Default)]
#[allow(unused)]
pub struct MockDiscovery {
    peers: HashMap<EndpointId, BTreeSet<SocketAddr>>,
}

impl MockDiscovery {
    pub fn new() -> Self {
        Self {
            peers: Default::default(),
        }
    }

    pub fn add_peer(&mut self, peer: &Endpoint) {
        self.peers
            .insert(peer.id(), peer.bound_sockets().into_iter().collect());
    }
}

impl Discovery for MockDiscovery {
    fn publish(&self, _data: &iroh::discovery::EndpointData) {}

    fn resolve(&self, id: EndpointId) -> Option<BoxStream<Result<DiscoveryItem, DiscoveryError>>> {
        let addr = self.peers.get(&id).unwrap();
        let info = EndpointInfo::new(id).with_ip_addrs(addr.clone());
        let item = DiscoveryItem::new(info, "mock", None);
        Some(Box::pin(once(Ok(item))))
    }
}

pub async fn create_endpoint(discovery: Option<MockDiscovery>) -> Endpoint {
    let mut builder = Endpoint::builder();
    if let Some(discovery) = discovery {
        builder = builder.discovery(discovery);
    }
    builder.bind().await.unwrap()
}

pub async fn create_endpoint_pair() -> (Endpoint, Endpoint) {
    let endpoint_1 = create_endpoint(None).await;
    let mut discovery = MockDiscovery::new();
    discovery.add_peer(&endpoint_1);
    let endpoint_2 = create_endpoint(Some(discovery)).await;
    (endpoint_1, endpoint_2)
}
