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
use indexmap::IndexMap;
use solana_client::rpc_client::RpcClient;
use solana_rpc_client_api::response::RpcContactInfo;
use solana_sdk::slot_history::Slot;
use tokio::time::sleep;
use tracing::{debug, error, info};

use crate::{errors::AtlasTxnSenderError, solana_rpc::SolanaRpc};

pub trait LeaderTracker: Send + Sync {
    /// get_leaders returns the next slot leaders in order
    fn get_leaders(&self) -> Vec<RpcContactInfo>;
}

const NUM_LEADERS_PER_SLOT: usize = 4;

#[derive(Clone)]
pub struct LeaderTrackerImpl {
    rpc_client: Arc<RpcClient>,
    solana_rpc: Arc<dyn SolanaRpc>,
    cur_slot: Arc<AtomicU64>,
    cur_leaders: Arc<DashMap<Slot, RpcContactInfo>>,
    num_leaders: usize,
    leader_offset: i64,
}

impl LeaderTrackerImpl {
    pub fn new(
        rpc_client: Arc<RpcClient>,
        solana_rpc: Arc<dyn SolanaRpc>,
        num_leaders: usize,
        leader_offset: i64,
    ) -> Self {
        let leader_tracker = Self {
            rpc_client,
            solana_rpc,
            cur_slot: Arc::new(AtomicU64::new(0)),
            cur_leaders: Arc::new(DashMap::new()),
            num_leaders,
            leader_offset,
        };
        leader_tracker.poll_slot();
        leader_tracker.poll_slot_leaders();
        leader_tracker
    }

    /// poll_slot polls for every new slot returned by gRPC geyser
    fn poll_slot(&self) {
        let solana_rpc = self.solana_rpc.clone();
        let cur_slot = self.cur_slot.clone();
        let leader_offset = self.leader_offset;
        tokio::spawn(async move {
            loop {
                let next_slot = solana_rpc.get_next_slot();
                let start_slot = next_slot.map(|s| _get_start_slot(s, leader_offset));
                if let Some(start_slot) = start_slot {
                    if start_slot > cur_slot.load(Ordering::Relaxed) {
                        cur_slot.store(start_slot, Ordering::Relaxed);
                    }
                }
            }
        });
    }

    /// poll_slot_leaders polls every minute for the next 1000 slot leaders and populates the cur_leaders map with the slot and ContactInfo of each leader
    fn poll_slot_leaders(&self) {
        let self_clone = self.clone();
        tokio::spawn(async move {
            loop {
                let start = Instant::now();
                if let Err(e) = self_clone.poll_slot_leaders_once() {
                    match e {
                        AtlasTxnSenderError::NoStartSlot => {
                            info!("Slot has not yet arrived from gRPC, trying again in 200ms");
                            sleep(Duration::from_millis(200)).await;
                        }
                        _ => {
                            error!("Error polling slot leaders: {}", e);
                            sleep(Duration::from_secs(1)).await;
                        }
                    }
                    continue;
                }
                statsd_time!("poll_slot_leaders", start.elapsed());
                sleep(Duration::from_secs(60)).await;
            }
        });
    }

    fn poll_slot_leaders_once(&self) -> Result<(), AtlasTxnSenderError> {
        let next_slot = self.cur_slot.load(Ordering::Relaxed);
        if next_slot == 0 {
            return Err(AtlasTxnSenderError::NoStartSlot);
        }
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
        self.clean_up_slot_leaders();
        info!(
            "Got leaders for slots {} to {}",
            next_slot,
            next_slot + slot_leaders.len() as u64
        );
        Ok(())
    }

    fn clean_up_slot_leaders(&self) {
        let cur_slot = self.cur_slot.load(Ordering::Relaxed);
        let mut slots_to_remove = vec![];
        for leaders in self.cur_leaders.iter() {
            if leaders.key().clone() < cur_slot {
                slots_to_remove.push(leaders.key().clone());
            }
        }
        for slot in slots_to_remove {
            self.cur_leaders.remove(&slot);
        }
    }
}

fn _get_start_slot(next_slot: u64, leader_offset: i64) -> u64 {
    let slot_buffer = leader_offset * (NUM_LEADERS_PER_SLOT as i64);
    let start_slot = if slot_buffer > 0 {
        next_slot + slot_buffer as u64
    } else {
        next_slot - slot_buffer.abs() as u64
    };
    start_slot
}

impl LeaderTracker for LeaderTrackerImpl {
    fn get_leaders(&self) -> Vec<RpcContactInfo> {
        let start_slot = self.cur_slot.load(Ordering::Relaxed);
        let end_slot = start_slot + (self.num_leaders * NUM_LEADERS_PER_SLOT) as u64;
        let mut leaders = IndexMap::new();
        for slot in start_slot..end_slot {
            let leader = self.cur_leaders.get(&slot);
            if let Some(leader) = leader {
                _ = leaders.insert(leader.pubkey.to_owned(), leader.value().to_owned());
            }
            if leaders.len() >= self.num_leaders {
                break;
            }
        }
        info!(
            "leaders: {:?}, start_slot: {:?}",
            leaders.clone().keys(),
            start_slot
        );
        leaders
            .values()
            .clone()
            .into_iter()
            .map(|v| v.to_owned())
            .collect()
    }
}
