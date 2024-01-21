use core::panic;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use cadence_macros::statsd_time;
use dashmap::DashMap;
use solana_client::rpc_client::RpcClient;
use solana_rpc_client_api::response::RpcContactInfo;
use solana_sdk::slot_history::Slot;
use tokio::time::sleep;
use tracing::{debug, error};

use crate::{errors::AtlasTxnSenderError, solana_rpc::SolanaRpc, DEFAULT_TPU_CONNECTION_POOL_SIZE};

pub trait LeaderTracker: Send + Sync {
    /// get_leaders returns the next slot leaders in order
    fn get_leaders(&self) -> Vec<RpcContactInfo>;
}

#[derive(Clone)]
pub struct LeaderTrackerImpl {
    rpc_client: Arc<RpcClient>,
    solana_rpc: Arc<dyn SolanaRpc>,
    cur_slot: Arc<AtomicU64>,
    cur_leaders: Arc<DashMap<Slot, RpcContactInfo>>,
}

impl LeaderTrackerImpl {
    pub fn new(rpc_client: Arc<RpcClient>, solana_rpc: Arc<dyn SolanaRpc>) -> Self {
        let leader_tracker = Self {
            rpc_client,
            solana_rpc,
            cur_slot: Arc::new(AtomicU64::new(0)),
            cur_leaders: Arc::new(DashMap::new()),
        };
        leader_tracker.poll_slot();
        leader_tracker.poll_slot_leaders();
        leader_tracker
    }

    /// poll_slot polls for every new slot returned by gRPC geyser
    fn poll_slot(&self) {
        let solana_rpc = self.solana_rpc.clone();
        let cur_slot = self.cur_slot.clone();
        tokio::spawn(async move {
            loop {
                let next_slot = solana_rpc.get_next_slot();
                if let Some(next_slot) = next_slot {
                    if next_slot > cur_slot.load(Ordering::Relaxed) {
                        cur_slot.store(next_slot, Ordering::Relaxed);
                    }
                }
            }
        });
    }

    /// poll_slot_leaders polls every minute for the next 1000 slot leaders and populates the cur_leaders map with the slot and ContactInfo of each leader
    fn poll_slot_leaders(&self) {
        let start_slot = self.rpc_client.get_slot();
        if let Err(e) = start_slot {
            panic!("Error getting current slot: {}", e);
        }
        let start_slot = start_slot.unwrap();
        self.cur_slot.store(start_slot, Ordering::Relaxed);
        let self_clone = self.clone();
        tokio::spawn(async move {
            loop {
                let start = Instant::now();
                if let Err(e) = self_clone.poll_slot_leaders_once() {
                    error!("Error polling slot leaders: {}", e);
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
                statsd_time!("poll_slot_leaders", start.elapsed());
                sleep(Duration::from_secs(60)).await;
            }
        });
    }

    fn poll_slot_leaders_once(&self) -> Result<(), AtlasTxnSenderError> {
        let next_slot = self.cur_slot.load(Ordering::Relaxed);
        debug!("Polling slot leaders for slot {}", next_slot);
        // polling 1000 slots ahead is more than enough
        let slot_leaders = self.rpc_client.get_slot_leaders(next_slot, 1000);
        if let Err(e) = slot_leaders {
            return Err(format!("Error getting slot leaders: {}", e).into());
        }
        let slot_leaders = slot_leaders.unwrap();
        let new_cluster_nodes = self.rpc_client.get_cluster_nodes();
        if let Err(e) = new_cluster_nodes {
            return Err(format!("Error getting cluster nodes: {}", e).into());
        }
        let new_cluster_nodes = new_cluster_nodes.unwrap();
        let mut cluster_node_map = HashMap::new();
        for node in new_cluster_nodes {
            cluster_node_map.insert(node.pubkey.clone(), node);
        }
        for (i, leader) in slot_leaders.iter().enumerate() {
            let contact_info = cluster_node_map.get(&leader.to_string());
            if let Some(contact_info) = contact_info {
                self.cur_leaders
                    .insert(next_slot + i as u64, contact_info.clone());
            } else {
                error!("Leader {} not found in cluster nodes", leader);
            }
        }
        Ok(())
    }
}

impl LeaderTracker for LeaderTrackerImpl {
    fn get_leaders(&self) -> Vec<RpcContactInfo> {
        let mut leaders = vec![];
        let cur_slot = self.cur_slot.load(Ordering::Relaxed);
        for slot in cur_slot..cur_slot + DEFAULT_TPU_CONNECTION_POOL_SIZE as u64 {
            let leader = self.cur_leaders.get(&slot);
            if let Some(leader) = leader {
                leaders.push(leader.value().clone());
            }
        }
        leaders
    }
}
