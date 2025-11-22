use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use futures_lite::StreamExt;
use iroh::{
    Endpoint, EndpointId,
    discovery::{Discovery, DiscoveryItem, EndpointData, EndpointInfo, IntoDiscovery},
};

#[derive(Debug, Default, Clone)]
pub struct MockDiscoveryMap {
    peers: Arc<RwLock<BTreeMap<EndpointId, Arc<EndpointData>>>>,
}

impl MockDiscoveryMap {
    #[inline]
    pub fn new() -> Self {
        Default::default()
    }

    pub async fn spawn_endpoint(&self) -> Endpoint {
        Endpoint::builder()
            .discovery(self.clone())
            .bind()
            .await
            .unwrap()
    }
}

impl IntoDiscovery for MockDiscoveryMap {
    fn into_discovery(
        self,
        endpoint: &iroh::Endpoint,
    ) -> Result<impl Discovery, iroh::discovery::IntoDiscoveryError> {
        Ok(MockDiscovery {
            id: endpoint.id(),
            map: self.clone(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct MockDiscovery {
    id: EndpointId,
    map: MockDiscoveryMap,
}

impl MockDiscovery {
    pub fn new(id: EndpointId, map: MockDiscoveryMap) -> Self {
        Self { id, map }
    }
}

impl Discovery for MockDiscovery {
    fn publish(&self, data: &EndpointData) {
        self.map
            .peers
            .write()
            .unwrap()
            .insert(self.id, Arc::new(data.clone()));
    }

    fn resolve(
        &self,
        endpoint_id: EndpointId,
    ) -> Option<
        futures_lite::stream::Boxed<
            Result<iroh::discovery::DiscoveryItem, iroh::discovery::DiscoveryError>,
        >,
    > {
        let data = self.map.peers.read().unwrap().get(&endpoint_id).cloned()?;

        let ip_addrs = data.ip_addrs().cloned().collect();

        let info = EndpointInfo::new(endpoint_id).with_ip_addrs(ip_addrs);

        let discovery_item = DiscoveryItem::new(info, "mock", None);

        Some(futures_lite::stream::once(Ok(discovery_item)).boxed())
    }
}
