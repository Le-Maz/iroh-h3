use std::{
    collections::BTreeMap,
    pin::Pin,
    sync::{Arc, RwLock},
};

use futures_lite::StreamExt;
use iroh::{
    Endpoint, EndpointId,
    address_lookup::{AddressLookup, EndpointData, EndpointInfo, IntoAddressLookup, Item},
};
use n0_future::Stream;

#[derive(Debug, Default, Clone)]
pub struct MockAddressLookupMap {
    peers: Arc<RwLock<BTreeMap<EndpointId, Arc<EndpointData>>>>,
}

impl MockAddressLookupMap {
    #[inline]
    pub fn new() -> Self {
        Default::default()
    }

    pub async fn spawn_endpoint(&self) -> Endpoint {
        Endpoint::builder()
            .address_lookup(self.clone())
            .bind()
            .await
            .unwrap()
    }
}

impl IntoAddressLookup for MockAddressLookupMap {
    fn into_address_lookup(
        self,
        endpoint: &Endpoint,
    ) -> Result<impl AddressLookup, iroh::address_lookup::IntoAddressLookupError> {
        Ok(MockAddressLookup {
            id: endpoint.id(),
            map: self.clone(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct MockAddressLookup {
    id: EndpointId,
    map: MockAddressLookupMap,
}

impl MockAddressLookup {
    pub fn new(id: EndpointId, map: MockAddressLookupMap) -> Self {
        Self { id, map }
    }
}

#[cfg(not(target_family = "wasm"))]
type BoxStream =
    Pin<Box<dyn Stream<Item = Result<Item, iroh::address_lookup::Error>> + Send + 'static>>;
#[cfg(target_family = "wasm")]
type BoxStream =
    Pin<Box<dyn Stream<Item = Result<DiscoveryItem, iroh::discovery::DiscoveryError>> + 'static>>;

impl AddressLookup for MockAddressLookup {
    fn publish(&self, data: &EndpointData) {
        self.map
            .peers
            .write()
            .unwrap()
            .insert(self.id, Arc::new(data.clone()));
    }

    fn resolve(&self, endpoint_id: EndpointId) -> Option<BoxStream> {
        let data = self.map.peers.read().unwrap().get(&endpoint_id).cloned()?;

        let ip_addrs = data.ip_addrs().cloned().collect();

        let info = EndpointInfo::new(endpoint_id).with_ip_addrs(ip_addrs);

        let discovery_item = Item::new(info, "mock", None);

        Some(futures_lite::stream::once(Ok(discovery_item)).boxed())
    }
}
