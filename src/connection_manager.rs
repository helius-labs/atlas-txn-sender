use std::{
    net::SocketAddr,
    net::{IpAddr, Ipv4Addr},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use solana_client::connection_cache::{ConnectionCache, NonblockingClientConnection};
use solana_sdk::signature::Keypair;

pub trait ConnectionManager: Send + Sync {
    fn get_nonblocking_connection(&self, addr: &SocketAddr) -> NonblockingClientConnection;
}

pub struct ConnectionManagerImpl {
    num_connections: usize,
    connection_caches: Arc<Vec<ConnectionCache>>,
    connection_index: Arc<AtomicU64>,
}

impl ConnectionManagerImpl {
    pub fn new_multi_with_identity(
        num_connections: usize,
        identity_keypair: Keypair,
        tpu_connection_pool_size: usize,
    ) -> Self {
        let mut connection_caches = Vec::new();
        for _ in 0..num_connections {
            connection_caches.push(ConnectionCache::new_with_client_options(
                "atlas-txn-sender",
                tpu_connection_pool_size,
                None, // created if none specified
                Some((&identity_keypair, IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)))),
                None, // not used as far as I can tell
            ));
        }
        Self {
            num_connections,
            connection_caches: Arc::new(connection_caches),
            connection_index: Arc::new(AtomicU64::new(0)),
        }
    }
    pub fn new_multi(num_connections: usize, tpu_connection_pool_size: usize) -> Self {
        let identity_keypair = Keypair::new();
        Self::new_multi_with_identity(num_connections, identity_keypair, tpu_connection_pool_size)
    }

    fn get_next_connection_index(&self) -> usize {
        let mut index = self.connection_index.fetch_add(1, Ordering::Relaxed);
        if index >= self.num_connections as u64 {
            index = 0;
            self.connection_index.store(0, Ordering::Relaxed);
        }
        index as usize
    }
    fn warm_connections(&self, addr: &SocketAddr) {
        for cache in self.connection_caches.iter() {
            cache.get_nonblocking_connection(addr);
        }
    }
}

impl ConnectionManager for ConnectionManagerImpl {
    fn get_nonblocking_connection(&self, addr: &SocketAddr) -> NonblockingClientConnection {
        self.warm_connections(addr);
        let index = self.get_next_connection_index();
        self.connection_caches[index].get_nonblocking_connection(addr)
    }
}
