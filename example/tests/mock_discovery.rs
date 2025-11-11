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
